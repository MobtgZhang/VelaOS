//! Network drivers: virtio-net (completion), e1000 (Intel), RTL8139 (Realtek).
//!
//! # What this crate is
//!
//! It implements NIC drivers for common QEMU-emulated network cards and
//! registers them as `net.device` instances so the socket layer can transmit
//! Ethernet frames.

#![no_std]

extern crate alloc;

use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, Ordering};
use spin::Mutex;

use vela_kernel_device::{cmd, errors, DeviceInfo, DriverOps, IORequest, IoState};
use vela_kernel_net::{self as net, transmit, EthernetHeader, MacAddr, NetDevice};

/// Helper: extract base class from class_code.
fn base_class(class_code: u32) -> u8 {
    ((class_code >> 16) & 0xFF) as u8
}

/// A virtio-net device instance (completes the stub in drivers/virtio-net).
pub struct VirtioNetDevice {
    mac: MacAddr,
    tx_queue: Mutex<Vec<Vec<u8>>>,
    link_up: AtomicU32,
}

impl VirtioNetDevice {
    pub fn new(mac: [u8; 6]) -> Arc<Self> {
        Arc::new(Self {
            mac: MacAddr(mac),
            tx_queue: Mutex::new(Vec::new()),
            link_up: AtomicU32::new(1),
        })
    }

    /// The MAC address this stub reports.
    pub fn mac(&self) -> MacAddr {
        self.mac
    }

    /// True when the link is up.
    pub fn link_up(&self) -> bool {
        self.link_up.load(Ordering::Acquire) != 0
    }
}

impl DriverOps for VirtioNetDevice {
    fn probe(&self, _info: &DeviceInfo) -> bool {
        true
    }

    fn open(&self, _device: u64, _unit: u32) -> Result<u64, i32> {
        Ok(0x5000_0000)
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

    fn remove(&self, _cookie: u64) {}
}

impl NetDevice for VirtioNetDevice {
    fn transmit(&self, frame: &[u8]) -> Result<usize, i32> {
        if self.link_up.load(Ordering::Acquire) == 0 {
            return Err(errors::NO_DEVICE);
        }
        if frame.len() < EthernetHeader::LEN || frame.len() > 1514 {
            return Err(errors::EMSGSIZE);
        }
        // In a real driver this would go through the virtio TX ring.
        // Here we just accept it and report success.
        self.tx_queue.lock().push(frame.to_vec());
        Ok(frame.len())
    }
}

/// Intel e1000 driver stub.
pub struct E1000Driver;

impl E1000Driver {
    pub fn new() -> Arc<Self> {
        Arc::new(Self)
    }
}

impl DriverOps for E1000Driver {
    fn probe(&self, info: &DeviceInfo) -> bool {
        // e1000: vendor 0x8086, network class 0x02.
        info.bus == "pci" && info.vendor_id == 0x8086 && base_class(info.class_code) == 0x02
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

/// Realtek RTL8139 driver stub.
pub struct Rtl8139Driver;

impl Rtl8139Driver {
    pub fn new() -> Arc<Self> {
        Arc::new(Self)
    }
}

impl DriverOps for Rtl8139Driver {
    fn probe(&self, info: &DeviceInfo) -> bool {
        // RTL8139: vendor 0x10EC, device 0x8139, network class.
        info.bus == "pci"
            && info.vendor_id == 0x10EC
            && info.device_id == 0x8139
            && base_class(info.class_code) == 0x02
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

/// Register all network drivers with the Device Manager.
pub fn register_all() {
    // Register stub NIC drivers so `OpenDevice` names them even when QEMU
    // attaches no such card.
    let e1000 = E1000Driver::new();
    vela_kernel_device::DeviceManager::register_driver(
        "net.e1000",
        Arc::clone(&e1000) as Arc<dyn DriverOps>,
    );
    vela_kernel_device::DeviceManager::register_device("net.e1000", 0, 0);

    let rtl8139 = Rtl8139Driver::new();
    vela_kernel_device::DeviceManager::register_driver(
        "net.rtl8139",
        Arc::clone(&rtl8139) as Arc<dyn DriverOps>,
    );
    vela_kernel_device::DeviceManager::register_device("net.rtl8139", 0, 0);

    // A synthetic virtio-net is the fallback only when the PCI enumeration
    // found no real NIC. The real driver is installed first by
    // `install_builtin_devices`, and `install_net_device` accepts exactly one
    // device, so blindly installing the synthetic one would silently discard
    // the real hardware path.
    if !net::device_installed() {
        let virtio: &'static Arc<VirtioNetDevice> = Box::leak(Box::new(VirtioNetDevice::new([
            0x52, 0x54, 0x00, 0x12, 0x34, 0x56,
        ])));
        vela_kernel_device::DeviceManager::register_driver(
            "net.virtio",
            Arc::clone(virtio) as Arc<dyn DriverOps>,
        );
        vela_kernel_device::DeviceManager::register_device("net.virtio", 0, 0x5000_0000);

        // SAFETY: this runs during boot, before any socket transmit, and the
        // leaked `Arc` lives for the rest of the boot.
        unsafe { net::install_net_device(&**virtio) };
    }
}

/// Get the MAC address of the primary network interface.
pub fn primary_mac() -> MacAddr {
    MacAddr([0x52, 0x54, 0x00, 0x12, 0x34, 0x56])
}

/// Send an Ethernet frame through the primary network interface.
pub fn send_frame(frame: &[u8]) -> Result<usize, i32> {
    transmit(frame)
}
