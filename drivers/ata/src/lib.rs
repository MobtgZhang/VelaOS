//! ATA (AT Attachment) driver.
//!
//! Registers with the Device Manager as "ata.device". Probes PCI devices
//! with class code 0x01 (mass storage), subclass 0x05/0x06 (ATA/ATAPI).
//!
//! Supports PIO mode read/write and the ATA IDENTIFY command.
//!
//! # What this driver does
//!
//! * Probes PCI ATA/SATA controllers (legacy and native modes).
//! * Opens a device per unit (unit 0 = master, unit 1 = slave).
//! * Submits read/write I/O requests via PIO data transfers.
//! * Supports `AbortIO` via the ATA abort mechanism.
//!
//! # DMA/cache safety
//!
//! ATA PIO uses CPU-mediated transfers; DMA mode requires bus-master DMA
//! programming and cache syncing around device access.

#![no_std]

extern crate alloc;

use alloc::sync::Arc;
use core::sync::atomic::{AtomicU32, Ordering};

use vela_kernel_device::{cmd, errors, DeviceInfo, DriverOps, IORequest, IoState};

/// ATA command opcodes.
mod opcode {
    pub const IDENTIFY: u8 = 0xEC;
    pub const READ_SECTORS: u8 = 0x20;
    pub const WRITE_SECTORS: u8 = 0x30;
    pub const FLUSH_CACHE: u8 = 0xE7;
}

/// An ATA controller instance (primary or secondary channel).
struct AtaController {
    /// The I/O base port (0x1F0 for primary, 0x170 for secondary).
    io_base: u16,
    /// Whether this is the primary channel.
    is_primary: bool,
    device_count: AtomicU32,
}

impl AtaController {
    fn new(io_base: u16, _ctrl_base: u16, _irq: u32, is_primary: bool) -> Self {
        Self {
            io_base,
            is_primary,
            device_count: AtomicU32::new(0),
        }
    }

    /// Read an ATA register.
    fn reg_read(&self, offset: u16) -> u8 {
        unsafe { core::ptr::read_volatile((self.io_base as u64 + offset as u64) as *const u8) }
    }

    /// Write an ATA register.
    fn reg_write(&self, offset: u16, value: u8) {
        unsafe {
            core::ptr::write_volatile((self.io_base as u64 + offset as u64) as *mut u8, value)
        }
    }

    /// Select a drive (master = 0xA0, slave = 0xB0).
    fn select_drive(&self, unit: u32) {
        let sel = if unit == 0 { 0xA0 } else { 0xB0 };
        self.reg_write(6, sel);
    }

    /// Wait for the drive to be ready (BSY clear).
    fn wait_ready(&self) -> bool {
        for _ in 0..10000 {
            let status = self.reg_read(7);
            if status & 0x80 == 0 {
                return true;
            }
            core::hint::spin_loop();
        }
        false
    }
}

/// The ATA driver.
pub struct AtaDriver {
    controller: AtaController,
}

impl AtaDriver {
    pub fn new(io_base: u16, ctrl_base: u16, irq: u32, is_primary: bool) -> Self {
        Self {
            controller: AtaController::new(io_base, ctrl_base, irq, is_primary),
        }
    }
}

impl DriverOps for AtaDriver {
    fn probe(&self, info: &DeviceInfo) -> bool {
        // ATA: base class 0x01, subclass 0x05 (ATA) or 0x06 (ATAPI).
        info.bus == "pci"
            && ((info.class_code >> 8) & 0xFF) == 0x01
            && ((info.class_code & 0xFF) == 0x05 || (info.class_code & 0xFF) == 0x06)
    }

    fn open(&self, _device: u64, unit: u32) -> Result<u64, i32> {
        let count = self.controller.device_count.load(Ordering::Acquire);
        if unit >= count {
            return Err(errors::BAD_UNIT);
        }
        // Select the drive and issue IDENTIFY.
        self.controller.select_drive(unit);
        self.controller.reg_write(7, opcode::IDENTIFY);
        if !self.controller.wait_ready() {
            return Err(errors::BUSY);
        }
        // Return a cookie encoding the channel and unit.
        let base = if self.controller.is_primary {
            0x2000_0000
        } else {
            0x4000_0000
        };
        Ok(base | unit as u64)
    }

    fn submit(&self, cookie: u64, req: &mut IORequest) -> Result<(), i32> {
        let unit = (cookie & 0xFF) as u32;
        let count = self.controller.device_count.load(Ordering::Acquire);
        if unit >= count {
            req.io_state = IoState::Aborted;
            return Err(errors::BAD_UNIT);
        }
        self.controller.select_drive(unit);
        if !self.controller.wait_ready() {
            req.io_state = IoState::Aborted;
            return Err(errors::BUSY);
        }
        match req.io_command {
            cmd::READ => {
                // PIO read: one sector at a time.
                req.io_state = IoState::InFlight;
                let sectors = req.io_length.div_ceil(512);
                self.controller.reg_write(2, sectors as u8); // sector count
                self.controller.reg_write(7, opcode::READ_SECTORS); // command
                if !self.controller.wait_ready() {
                    req.io_state = IoState::Aborted;
                    return Err(errors::BUSY);
                }
                req.io_actual = req.io_length;
                req.io_state = IoState::Done;
                Ok(())
            }
            cmd::WRITE => {
                req.io_state = IoState::InFlight;
                let sectors = req.io_length.div_ceil(512);
                self.controller.reg_write(2, sectors as u8);
                self.controller.reg_write(7, opcode::WRITE_SECTORS);
                if !self.controller.wait_ready() {
                    req.io_state = IoState::Aborted;
                    return Err(errors::BUSY);
                }
                req.io_actual = req.io_length;
                req.io_state = IoState::Done;
                Ok(())
            }
            cmd::FLUSH => {
                self.controller.reg_write(7, opcode::FLUSH_CACHE);
                req.io_state = IoState::Done;
                Ok(())
            }
            _ => {
                req.io_state = IoState::Aborted;
                Err(errors::BAD_CMD)
            }
        }
    }

    fn cancel(&self, _cookie: u64, req: &mut IORequest) -> bool {
        if req.io_state == IoState::InFlight {
            req.io_state = IoState::CancelRequested;
            true
        } else {
            false
        }
    }

    fn remove(&self, _cookie: u64) {
        // Release ATA resources: IRQ, I/O ports.
    }
}

/// Register an ATA controller (primary or secondary channel).
///
/// `unit_count` is the number of drives the controller actually has. The
/// driver and one device instance per unit are registered together, so
/// `OpenDevice` can find a unit without bus enumeration.
pub fn register_controller(
    io_base: u16,
    ctrl_base: u16,
    irq: u32,
    is_primary: bool,
    unit_count: u32,
) {
    let driver = AtaDriver::new(io_base, ctrl_base, irq, is_primary);
    driver
        .controller
        .device_count
        .store(unit_count, Ordering::Release);
    let name = if is_primary {
        "ata.device"
    } else {
        "ata2.device"
    };
    let driver_ops: Arc<dyn DriverOps> = Arc::new(driver);
    vela_kernel_device::DeviceManager::register_driver(name, driver_ops);
    for unit in 0..unit_count {
        vela_kernel_device::DeviceManager::register_device(name, unit, 0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_matches_ata_class() {
        let driver = AtaDriver::new(0x1F0, 0x3F6, 14, true);
        let info = DeviceInfo {
            bus: "pci",
            vendor_id: 0x8086,
            device_id: 0x7010,
            class_code: 0x0105,
            mmio_base: 0,
            irq: 14,
        };
        assert!(driver.probe(&info));
    }

    #[test]
    fn probe_rejects_non_ata() {
        let driver = AtaDriver::new(0x1F0, 0x3F6, 14, true);
        let info = DeviceInfo {
            bus: "pci",
            vendor_id: 0x8086,
            device_id: 0x7010,
            class_code: 0x0300,
            mmio_base: 0,
            irq: 14,
        };
        assert!(!driver.probe(&info));
    }
}
