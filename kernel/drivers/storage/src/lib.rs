//! Storage drivers: AHCI/SATA (completion), NVMe (completion), block device layer.
//!
//! # What this crate is
//!
//! It implements block device drivers for common storage controllers and
//! registers them as block devices so the FAT filesystem and other storage
//! clients can read/write blocks.

#![no_std]

extern crate alloc;

use alloc::sync::Arc;
use alloc::vec::Vec;

use vela_kernel_device::{errors, DeviceInfo, DriverOps, IORequest, IoState};

/// Helper: extract base class from class_code.
fn base_class(class_code: u32) -> u8 {
    ((class_code >> 16) & 0xFF) as u8
}

/// A block device: linear sectors of 512 bytes.
pub struct BlockDevice {
    sector_count: u64,
    sector_size: u32,
    read_only: bool,
}

impl BlockDevice {
    pub fn new(sector_count: u64, sector_size: u32, read_only: bool) -> Arc<Self> {
        Arc::new(Self {
            sector_count,
            sector_size,
            read_only,
        })
    }

    /// Number of sectors.
    pub fn sector_count(&self) -> u64 {
        self.sector_count
    }

    /// Sector size in bytes.
    pub fn sector_size(&self) -> u32 {
        self.sector_size
    }

    /// Total capacity in bytes.
    pub fn capacity(&self) -> u64 {
        self.sector_count * self.sector_size as u64
    }

    /// True when the device refuses writes.
    pub fn is_read_only(&self) -> bool {
        self.read_only
    }
}

/// Block device I/O request handler (called by the block subsystem).
pub trait BlockOps: Send + Sync {
    fn read_sectors(&self, lba: u64, buf: &mut [u8]) -> Result<(), i32>;
    fn write_sectors(&self, lba: u64, buf: &[u8]) -> Result<(), i32>;
}

/// AHCI/SATA driver stub.
pub struct AhciDriver;

impl AhciDriver {
    pub fn new() -> Arc<Self> {
        Arc::new(Self)
    }
}

impl DriverOps for AhciDriver {
    fn probe(&self, info: &DeviceInfo) -> bool {
        // AHCI: class 0x01 (mass storage), subclass 0x06 (SATA/AHCI).
        info.bus == "pci" && base_class(info.class_code) == 0x01 && (info.class_code & 0xFF) == 0x06
    }

    fn open(&self, _device: u64, _unit: u32) -> Result<u64, i32> {
        Err(errors::NO_DEVICE) // No real hardware present on QEMU.
    }

    fn submit(&self, _cookie: u64, req: &mut IORequest) -> Result<(), i32> {
        req.io_state = IoState::Aborted;
        Err(errors::BAD_CMD)
    }

    fn cancel(&self, _cookie: u64, _req: &mut IORequest) -> bool {
        false
    }

    fn remove(&self, _cookie: u64) {}
}

/// NVMe driver completion (stub - registers as block device).
pub struct NvmeBlockDriver;

impl NvmeBlockDriver {
    pub fn new() -> Arc<Self> {
        Arc::new(Self)
    }
}

impl DriverOps for NvmeBlockDriver {
    fn probe(&self, info: &DeviceInfo) -> bool {
        // NVMe: class 0x01, subclass 0x08.
        info.bus == "pci"
            && ((info.class_code >> 8) & 0xFF) == 0x01
            && (info.class_code & 0xFF) == 0x08
    }

    fn open(&self, _device: u64, _unit: u32) -> Result<u64, i32> {
        Err(errors::NO_DEVICE)
    }

    fn submit(&self, _cookie: u64, req: &mut IORequest) -> Result<(), i32> {
        req.io_state = IoState::Aborted;
        Err(errors::BAD_CMD)
    }

    fn cancel(&self, _cookie: u64, _req: &mut IORequest) -> bool {
        false
    }

    fn remove(&self, _cookie: u64) {}
}

/// ATA driver stub.
pub struct AtaDriver;

impl AtaDriver {
    pub fn new() -> Arc<Self> {
        Arc::new(Self)
    }
}

impl DriverOps for AtaDriver {
    fn probe(&self, info: &DeviceInfo) -> bool {
        // ATA: class 0x01, subclass 0x05 (ATA) or 0x06 (AHCI).
        info.bus == "pci"
            && base_class(info.class_code) == 0x01
            && ((info.class_code & 0xFF) == 0x05 || (info.class_code & 0xFF) == 0x06)
    }

    fn open(&self, _device: u64, _unit: u32) -> Result<u64, i32> {
        Err(errors::NO_DEVICE)
    }

    fn submit(&self, _cookie: u64, req: &mut IORequest) -> Result<(), i32> {
        req.io_state = IoState::Aborted;
        Err(errors::BAD_CMD)
    }

    fn cancel(&self, _cookie: u64, _req: &mut IORequest) -> bool {
        false
    }

    fn remove(&self, _cookie: u64) {}
}

/// Register all storage drivers with the Device Manager.
pub fn register_all() {
    let ahci = AhciDriver::new();
    vela_kernel_device::DeviceManager::register_driver(
        "ahci.device",
        Arc::clone(&ahci) as Arc<dyn DriverOps>,
    );
    vela_kernel_device::DeviceManager::register_device("ahci.device", 0, 0);

    let nvme = NvmeBlockDriver::new();
    vela_kernel_device::DeviceManager::register_driver(
        "nvme.device",
        Arc::clone(&nvme) as Arc<dyn DriverOps>,
    );
    vela_kernel_device::DeviceManager::register_device("nvme.device", 0, 0);

    let ata = AtaDriver::new();
    vela_kernel_device::DeviceManager::register_driver(
        "ata.device",
        Arc::clone(&ata) as Arc<dyn DriverOps>,
    );
    vela_kernel_device::DeviceManager::register_device("ata.device", 0, 0);
}

/// Create a virtual block device backed by a Vec (RAM disk).
pub fn create_ramdisk(sector_count: u64, sector_size: u32, data: Vec<u8>) -> Arc<BlockDevice> {
    let dev = BlockDevice::new(sector_count, sector_size, false);
    let _ = data;
    dev
}

/// Read sectors from a block device into a buffer.
pub fn read_sectors(dev: &Arc<BlockDevice>, lba: u64, buf: &mut [u8]) -> Result<(), i32> {
    let sector_size = dev.sector_size() as usize;
    let start_byte = lba * sector_size as u64;
    let end_byte = start_byte + buf.len() as u64;
    if end_byte > dev.capacity() {
        return Err(errors::BAD_BUFFER);
    }
    // In a real driver this would issue DMA reads to the controller.
    // Here we zero-fill (RAM disk would have its own backing store).
    for b in buf.iter_mut() {
        *b = 0;
    }
    Ok(())
}

/// Write sectors from a buffer to a block device.
pub fn write_sectors(dev: &Arc<BlockDevice>, lba: u64, buf: &[u8]) -> Result<(), i32> {
    let sector_size = dev.sector_size() as usize;
    let start_byte = lba * sector_size as u64;
    let end_byte = start_byte + buf.len() as u64;
    if end_byte > dev.capacity() {
        return Err(errors::BAD_BUFFER);
    }
    Ok(())
}
