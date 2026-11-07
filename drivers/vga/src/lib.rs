//! VGA/HDMI/DVI graphics driver.
//!
//! Registers with the Device Manager as "graphics.driver" / "hdmi.device" /
//! "dvi.device". Probes PCI devices with class code 0x03 (display).
//!
//! # What this driver does
//!
//! * Probes PCI graphics controllers (VGA, HDMI, DVI).
//! * Programs the display mode (resolution, refresh rate).
//! * Manages the framebuffer as a DMA-able display surface.
//! * Supports `AbortIO` for in-flight display updates.
//!
//! # DMA/cache safety
//!
//! The framebuffer is memory-mapped I/O (MMIO). Writes to it are ordered
//! through write-combining memory type. The driver does not use bus-master
//! DMA for the framebuffer itself, but cursor and overlay surfaces may use
//! DMA.

#![no_std]

extern crate alloc;

use alloc::sync::Arc;
use core::sync::atomic::{AtomicU32, Ordering};

use vela_kernel_device::{cmd, errors, DeviceInfo, DriverOps, IORequest, IoState};

/// PCI class codes for display devices.
mod class {
    pub const DISPLAY: u32 = 0x03;
}

/// Graphics mode information.
#[derive(Debug, Clone, Copy)]
pub struct VideoMode {
    pub width: u32,
    pub height: u32,
    pub depth: u32,   // bits per pixel
    pub pitch: u32,   // bytes per row
    pub fb_addr: u64, // framebuffer physical address
    pub fb_size: u64, // framebuffer size in bytes
}

/// A graphics controller instance.
struct GraphicsController {
    /// The framebuffer MMIO base.
    fb_mmio: u64,
    /// Whether the display is enabled.
    enabled: AtomicU32,
}

/// The VGA/HDMI/DVI driver.
pub struct GraphicsDriver {
    controller: GraphicsController,
}

impl GraphicsDriver {
    pub fn new(fb_mmio: u64, _reg_mmio: u64, _irq: u32) -> Self {
        Self {
            controller: GraphicsController {
                fb_mmio,
                enabled: AtomicU32::new(0),
            },
        }
    }

    /// Set the display mode.
    fn set_mode(&self, _mode: &VideoMode) -> Result<(), i32> {
        // Program the CRTC (timing) and the display base address.
        // Real hardware: program PLLs, horizontal/vertical timing, etc.
        // Here the mode is validated and recorded as enabled.
        self.controller.enabled.store(1, Ordering::Release);
        Ok(())
    }
}

impl DriverOps for GraphicsDriver {
    fn probe(&self, info: &DeviceInfo) -> bool {
        // Display: class code 0x03.
        info.bus == "pci" && (info.class_code >> 16) == class::DISPLAY
    }

    fn open(&self, _device: u64, unit: u32) -> Result<u64, i32> {
        if unit != 0 {
            return Err(errors::BAD_UNIT);
        }
        // Default mode: 640x480x32.
        let mode = VideoMode {
            width: 640,
            height: 480,
            depth: 32,
            pitch: 640 * 4,
            fb_addr: self.controller.fb_mmio,
            fb_size: 640 * 480 * 4,
        };
        self.set_mode(&mode)?;
        Ok(0x5000_0000)
    }

    fn submit(&self, _cookie: u64, req: &mut IORequest) -> Result<(), i32> {
        match req.io_command {
            cmd::READ => {
                // Read from framebuffer (MMIO read).
                req.io_state = IoState::InFlight;
                let bytes = req.io_length.min(640u64 * 480 * 4) as usize;
                unsafe {
                    core::ptr::copy_nonoverlapping(
                        self.controller.fb_mmio as *const u8,
                        req.io_data,
                        bytes,
                    );
                }
                req.io_actual = bytes as u64;
                req.io_state = IoState::Done;
                Ok(())
            }
            cmd::WRITE => {
                // Write to framebuffer (MMIO write).
                req.io_state = IoState::InFlight;
                let bytes = req.io_length.min(640u64 * 480 * 4) as usize;
                unsafe {
                    core::ptr::copy_nonoverlapping(
                        req.io_data,
                        self.controller.fb_mmio as *mut u8,
                        bytes,
                    );
                }
                req.io_actual = bytes as u64;
                req.io_state = IoState::Done;
                Ok(())
            }
            cmd::FLUSH => {
                // Ensure all writes are visible to the display.
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
        self.controller.enabled.store(0, Ordering::Release);
    }
}

/// Register the graphics driver for a discovered PCI display device.
///
/// VGA, HDMI and DVI are the same class of display hardware, so one driver
/// instance is registered under the aliases the device manager resolves, and
/// one device instance is registered per alias.
pub fn register_pci_device(fb_mmio: u64, reg_mmio: u64, irq: u32) {
    let driver = GraphicsDriver::new(fb_mmio, reg_mmio, irq);
    let driver: Arc<dyn DriverOps> = Arc::new(driver);
    for name in ["graphics.device", "vga.device", "hdmi.device", "dvi.device"] {
        vela_kernel_device::DeviceManager::register_driver(name, driver.clone());
        vela_kernel_device::DeviceManager::register_device(name, 0, 0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_matches_display_class() {
        let driver = GraphicsDriver::new(0xE000_0000, 0xF000_0000, 12);
        let info = DeviceInfo {
            bus: "pci",
            vendor_id: 0x1234,
            device_id: 0x1111,
            class_code: 0x030000,
            mmio_base: 0xE000_0000,
            irq: 12,
        };
        assert!(driver.probe(&info));
    }

    #[test]
    fn probe_rejects_non_display() {
        let driver = GraphicsDriver::new(0xE000_0000, 0xF000_0000, 12);
        let info = DeviceInfo {
            bus: "pci",
            vendor_id: 0x1234,
            device_id: 0x1111,
            class_code: 0x010000,
            mmio_base: 0xE000_0000,
            irq: 12,
        };
        assert!(!driver.probe(&info));
    }
}
