//! virtio-net driver.
//!
//! # What this crate is
//!
//! It registers virtio-net NICs discovered on the PCI bus and exposes them as
//! `NetDevice` implementations so the kernel's network stack and BSD sockets
//! can transmit Ethernet frames.
//!
//! # What is implemented and what is not
//!
//! The PCI *discovery* is real: vendor `0x1AF4` and the virtio network device
//! IDs are checked against the enumerated bus. The transmit path is a queue in
//! the driver rather than a virtqueue ring, because the kernel does not yet
//! have a DMA-backed virtqueue setup. This is enough for the boot-time network
//! path (frames are accepted and counted) and is the registration point where
//! a real ring can be wired in later.
//!
//! The MAC address is synthetic per function. Reading it from the virtio
//! configuration space requires mapping the device's BAR and parsing the
//! modern capability layout, which is deliberately left to the future driver.

#![no_std]

extern crate alloc;

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, Ordering};
use spin::Mutex;

use vela_arch::pci::PciDevice;
use vela_kernel_device::{cmd, errors, DeviceInfo, DriverOps, IORequest, IoState};
use vela_kernel_net::{EthernetHeader, MacAddr, NetDevice};

/// virtio network device ID, transitional.
pub const VIRTIO_NET_LEGACY: u16 = 0x1000;
/// virtio network device ID, modern.
pub const VIRTIO_NET_MODERN: u16 = 0x1041;
/// virtio PCI vendor ID.
pub const VIRTIO_VENDOR: u16 = 0x1AF4;

/// One virtio-net function.
pub struct VirtioNet {
    mac: MacAddr,
    bus: u8,
    device: u8,
    function: u8,
    link_up: AtomicU32,
    tx_queue: Mutex<Vec<Vec<u8>>>,
}

impl VirtioNet {
    /// Build a driver for one PCI function.
    pub fn new(bus: u8, device: u8, function: u8) -> Arc<Self> {
        // A synthetic locally-administered MAC, unique per function.
        let mac = MacAddr([0x52, 0x54, bus, device, function, 0x01]);
        Arc::new(Self {
            mac,
            bus,
            device,
            function,
            link_up: AtomicU32::new(1),
            tx_queue: Mutex::new(Vec::new()),
        })
    }

    /// The MAC address this interface reports.
    pub fn mac(&self) -> MacAddr {
        self.mac
    }

    /// The PCI location of this function.
    pub fn location(&self) -> (u8, u8, u8) {
        (self.bus, self.device, self.function)
    }

    /// True when the link is up.
    pub fn link_up(&self) -> bool {
        self.link_up.load(Ordering::Acquire) != 0
    }

    /// Drop the link (used when the device is removed).
    pub fn set_link_down(&self) {
        self.link_up.store(0, Ordering::Release);
    }

    /// How many frames have been queued.
    pub fn queued_frames(&self) -> usize {
        self.tx_queue.lock().len()
    }
}

impl DriverOps for VirtioNet {
    fn probe(&self, info: &DeviceInfo) -> bool {
        info.bus == "pci"
            && info.vendor_id == VIRTIO_VENDOR as u32
            && (info.device_id == VIRTIO_NET_LEGACY as u32
                || info.device_id == VIRTIO_NET_MODERN as u32)
    }

    fn open(&self, _device: u64, _unit: u32) -> Result<u64, i32> {
        Ok(0x5000_0000
            + ((self.bus as u64) << 16)
            + ((self.device as u64) << 8)
            + self.function as u64)
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
        self.set_link_down();
    }
}

impl NetDevice for VirtioNet {
    fn transmit(&self, frame: &[u8]) -> Result<usize, i32> {
        if !self.link_up() {
            return Err(errors::NO_DEVICE);
        }
        if frame.len() < EthernetHeader::LEN || frame.len() > 1514 {
            return Err(errors::EMSGSIZE);
        }
        // The virtqueue ring is not implemented yet; the frame is accepted
        // into the driver's queue so the network path has a real endpoint.
        self.tx_queue.lock().push(frame.to_vec());
        Ok(frame.len())
    }
}

/// Register every virtio-net PCI function found by the bus scan.
///
/// Returns the discovered devices so the boot path can install the first one
/// as the global transmit device.
pub fn register_pci_devices(devices: &[PciDevice]) -> Vec<Arc<VirtioNet>> {
    let mut result = Vec::new();
    for device in devices {
        if device.vendor_id != VIRTIO_VENDOR {
            continue;
        }
        if device.device_id != VIRTIO_NET_LEGACY && device.device_id != VIRTIO_NET_MODERN {
            continue;
        }
        if device.base_class() != 0x02 {
            continue;
        }

        let net = VirtioNet::new(device.bus, device.device, device.function);
        let name = match device.device_id {
            VIRTIO_NET_LEGACY => "virtio-net.legacy",
            _ => "virtio-net.modern",
        };
        vela_kernel_device::DeviceManager::register_driver(
            name,
            Arc::clone(&net) as Arc<dyn DriverOps>,
        );
        vela_kernel_device::DeviceManager::register_device(name, 0, 0x5000_0000);
        result.push(net);
    }
    result
}
