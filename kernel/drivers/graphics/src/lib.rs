//! Graphics drivers: VGA/VESA framebuffer + stubs for AMD, Intel, Nouveau/NVIDIA.
//!
//! # What this crate is
//!
//! It registers display drivers with the Device Manager so the graphics stack
//! can map the framebuffer and composite to it. The primary driver for QEMU
//! is the VESA/VGA framebuffer driver, which reads the display geometry from
//! the boot handoff (GOP) and exposes a linear framebuffer.
//!
//! AMD, Intel and Nouveau/NVIDIA drivers are stubs that register as devices
//! but report "no hardware present" on QEMU. They provide the registration
//! points so real hardware can be wired in later.

#![no_std]

extern crate alloc;

use alloc::sync::Arc;
use core::sync::atomic::{AtomicU32, Ordering};

use vela_kernel_device::{cmd, errors, DeviceInfo, DriverOps, IORequest, IoState};
use vela_kernel_graphics::Graphics;

/// Helper: extract base class from class_code.
fn base_class(class_code: u32) -> u8 {
    ((class_code >> 16) & 0xFF) as u8
}

/// Framebuffer information from the boot handoff.
pub struct FramebufferInfo {
    pub address: u64,
    pub width: u32,
    pub height: u32,
    pub stride_bytes: u32,
    pub format: u32, // PixelFormat from boot protocol
}

/// The VGA/VESA framebuffer driver.
pub struct VgaDriver {
    fb: FramebufferInfo,
    enabled: AtomicU32,
}

impl VgaDriver {
    pub fn new(fb: FramebufferInfo) -> Arc<Self> {
        Arc::new(Self {
            fb,
            enabled: AtomicU32::new(0),
        })
    }

    /// The framebuffer geometry this driver owns.
    pub fn framebuffer(&self) -> &FramebufferInfo {
        &self.fb
    }
}

impl DriverOps for VgaDriver {
    fn probe(&self, _info: &DeviceInfo) -> bool {
        // VGA/VESA is always present (the bootloader gave us a framebuffer).
        true
    }

    fn open(&self, _device: u64, _unit: u32) -> Result<u64, i32> {
        if self
            .enabled
            .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Err(errors::BUSY);
        }
        Ok(0x3000_0000)
    }

    fn submit(&self, _cookie: u64, req: &mut IORequest) -> Result<(), i32> {
        match req.io_command {
            cmd::STATUS => {
                req.io_state = IoState::Done;
                req.io_actual = 0;
                req.io_error = errors::OK;
                Ok(())
            }
            _ => {
                req.io_state = IoState::Aborted;
                Err(errors::BAD_CMD)
            }
        }
    }

    fn cancel(&self, _cookie: u64, _req: &mut IORequest) -> bool {
        false
    }

    fn remove(&self, _cookie: u64) {
        self.enabled.store(0, Ordering::Release);
    }
}

/// AMD graphics driver stub.
pub struct AmdGpuDriver;

impl AmdGpuDriver {
    pub fn new() -> Arc<Self> {
        Arc::new(Self)
    }
}

impl DriverOps for AmdGpuDriver {
    fn probe(&self, info: &DeviceInfo) -> bool {
        // AMD: vendor 0x1002, display class 0x03.
        info.bus == "pci" && info.vendor_id == 0x1002 && base_class(info.class_code) == 0x03
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

/// Intel graphics driver stub.
pub struct IntelGpuDriver;

impl IntelGpuDriver {
    pub fn new() -> Arc<Self> {
        Arc::new(Self)
    }
}

impl DriverOps for IntelGpuDriver {
    fn probe(&self, info: &DeviceInfo) -> bool {
        // Intel: vendor 0x8086, display class 0x03.
        info.bus == "pci" && info.vendor_id == 0x8086 && base_class(info.class_code) == 0x03
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

/// Nouveau/NVIDIA graphics driver stub.
pub struct NouveauGpuDriver;

impl NouveauGpuDriver {
    pub fn new() -> Arc<Self> {
        Arc::new(Self)
    }
}

impl DriverOps for NouveauGpuDriver {
    fn probe(&self, info: &DeviceInfo) -> bool {
        // NVIDIA: vendor 0x10DE, display class 0x03.
        info.bus == "pci" && info.vendor_id == 0x10DE && base_class(info.class_code) == 0x03
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

/// Register all graphics drivers with the Device Manager.
///
/// Call this from kernel/entry after PCI enumeration and after the
/// framebuffer geometry is known from the boot handoff.
pub fn register_all(fb: Option<FramebufferInfo>) {
    // Register the VGA/VESA framebuffer driver if we have a framebuffer.
    if let Some(fb) = fb {
        let drv = VgaDriver::new(fb);
        vela_kernel_device::DeviceManager::register_driver(
            "vgavga.device",
            Arc::clone(&drv) as Arc<dyn DriverOps>,
        );
        vela_kernel_device::DeviceManager::register_device("vgavga.device", 0, 0x3000_0000);
    }

    // Register stub drivers for AMD, Intel, Nouveau/NVIDIA.
    let amd = AmdGpuDriver::new();
    vela_kernel_device::DeviceManager::register_driver(
        "amdgpu.device",
        Arc::clone(&amd) as Arc<dyn DriverOps>,
    );
    vela_kernel_device::DeviceManager::register_device("amdgpu.device", 0, 0);

    let intel = IntelGpuDriver::new();
    vela_kernel_device::DeviceManager::register_driver(
        "intelgpu.device",
        Arc::clone(&intel) as Arc<dyn DriverOps>,
    );
    vela_kernel_device::DeviceManager::register_device("intelgpu.device", 0, 0);

    let nouveau = NouveauGpuDriver::new();
    vela_kernel_device::DeviceManager::register_driver(
        "nouveau.device",
        Arc::clone(&nouveau) as Arc<dyn DriverOps>,
    );
    vela_kernel_device::DeviceManager::register_device("nouveau.device", 0, 0);
}

/// Map the framebuffer into the graphics subsystem.
///
/// Returns the graphics `Graphics` service with the framebuffer registered
/// as a raster so drawing commands can target it.
pub fn init_graphics(fb: &FramebufferInfo) -> Arc<Graphics> {
    let g = Arc::new(Graphics::new());
    // Register the screen-sized raster with the graphics service.
    let _ = g.register_raster(fb.width, fb.height, 32);
    g
}
