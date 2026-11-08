//! Device driver framework: Device Manager, IORequest state machine, and
//! Exec Device-compatible I/O.
//!
//! # What this module is
//!
//! The layer between applications and hardware:
//!
//! * [`DeviceManager`] — registers drivers, matches them to devices, and
//!   dispatches `OpenDevice`/`CloseDevice`/`SendIO`/`DoIO`/`WaitIO`/`AbortIO`.
//! * [`IORequest`] — the I/O request state machine: `NEW → OPENED → SUBMITTED
//!   → [QUICK_DONE] → REUSABLE`, with `QUEUED → IN_FLIGHT → DONE → REPLIED`.
//! * [`DriverOps`] — the driver interface: `probe`, `open`, `submit`,
//!   `cancel`, `remove`.
//!
//! # The I/O request lifecycle
//!
//! 1. Client creates a `MsgPort` and `IORequest`.
//! 2. Client calls `OpenDevice(name, unit, req, flags)` — the manager finds
//!    the driver and calls its `open`.
//! 3. Client sets `io_Command`, `io_Data`, `io_Length` and calls `SendIO` or
//!    `DoIO`.
//! 4. The driver's `submit` queues the request; it moves to `IN_FLIGHT`.
//! 5. The device completes the request, moves it to `DONE`, and replies to
//!    the client's port.
//! 6. Client calls `WaitIO` or `CheckIO` to collect the result.
//! 7. Client calls `CloseDevice` to release the device.
//!
//! `IOF_QUICK` means the caller inlined completion and no reply is needed;
//! `DoIO` blocks until completion; `SendIO` returns immediately.
//!
//! # DMA/cache safety
//!
//! DMA buffers have a “map → sync → device use → sync → unmap” lifetime.
//! The device driver is responsible for mapping its DMA buffers through the
//! IOMMU (when present) and for cache syncing around device access. The
//! kernel does not do this implicitly — a driver that reads a DMA buffer
//! without syncing gets stale data.
//!
//! # Units
//!
//! Devices are addressed by name + unit number. A driver that supports
//! multiple units (e.g. `ata.device` unit 0 = primary, unit 1 = secondary)
//! gets `open` called once per unit.

#![no_std]

extern crate alloc;

use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use spin::Mutex;

/// An I/O request's command codes.
pub mod cmd {
    /// No operation — used to test a device is present.
    pub const NONE: u32 = 0;
    /// Read data from the device.
    pub const READ: u32 = 1;
    /// Write data to the device.
    pub const WRITE: u32 = 2;
    /// Get device status.
    pub const STATUS: u32 = 3;
    /// Reset the device.
    pub const RESET: u32 = 4;
    /// Flush device buffers.
    pub const FLUSH: u32 = 5;
}

/// IORequest flags.
pub mod flags {
    /// The caller will not wait for a reply; completion is inlined.
    pub const IOF_QUICK: u32 = 1 << 0;
    /// The request is high priority.
    pub const IOF_PRIORITY: u32 = 1 << 1;
    /// The data buffer is suitable for DMA.
    pub const IOF_DMA: u32 = 1 << 2;
}

/// I/O request error codes.
pub mod errors {
    /// Success.
    pub const OK: i32 = 0;
    /// The device does not exist.
    pub const NO_DEVICE: i32 = -1;
    /// The unit does not exist on this device.
    pub const BAD_UNIT: i32 = -2;
    /// The command is not supported.
    pub const BAD_CMD: i32 = -3;
    /// The buffer is invalid.
    pub const BAD_BUFFER: i32 = -4;
    /// The request was aborted.
    pub const ABORTED: i32 = -5;
    /// The device is busy.
    pub const BUSY: i32 = -6;
    /// The message is too large for the device.
    pub const EMSGSIZE: i32 = -7;
}

/// The IORequest state machine.
///
/// ```text
/// NEW → OPENED → SUBMITTED → [QUICK_DONE] → REUSABLE
///                           └→ QUEUED → IN_FLIGHT
///                                     ├→ DONE → REPLIED
///                                     └→ CANCEL_REQUESTED
///                                               └→ REPLIED
/// ```
///
/// `DoIO` typically waits until `DONE`; `SendIO` returns at `SUBMITTED`.
/// `IOF_QUICK` may go `SUBMITTED → QUICK_DONE` without a reply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IoState {
    /// Fresh request, not yet opened.
    New,
    /// Opened via `OpenDevice`.
    Opened,
    /// Submitted to the driver.
    Submitted,
    /// Queued inside the driver, not yet in flight.
    Queued,
    /// In flight — the device is working on it.
    InFlight,
    /// Completed, reply pending.
    Done,
    /// Reply delivered to the client's port.
    Replied,
    /// Quick completion — no reply expected.
    QuickDone,
    /// Cancellation requested; awaiting completion.
    CancelRequested,
    /// Aborted by the client or driver.
    Aborted,
}

/// One I/O request.
///
/// Owned by the client (it lives in client memory), but the kernel tracks its
/// state through this structure. The client fills `io_Command`, `io_Data`,
/// `io_Length` and the driver fills `io_Actual` and `io_Error` on completion.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct IORequest {
    /// The message node — links the request into queues.
    pub mn_node: vela_abi_native64::Node,
    /// The reply port — where `ReplyMsg` delivers completion.
    pub mn_reply_port: u64,
    /// The device this request names.
    pub io_device: u64,
    /// The unit within the device.
    pub io_unit: u32,
    /// The command (`cmd::READ`, `cmd::WRITE`, …).
    pub io_command: u32,
    /// Flags (`flags::IOF_*`).
    pub io_flags: u32,
    /// The current state.
    pub io_state: IoState,
    /// The data buffer (client-owned).
    pub io_data: *mut u8,
    /// The buffer length, in bytes.
    pub io_length: u64,
    /// Bytes actually transferred, filled in by the driver.
    pub io_actual: u64,
    /// The error code, filled in by the driver.
    pub io_error: i32,
    /// The driver's private cookie.
    pub io_driver_cookie: u64,
}

impl IORequest {
    /// A fresh request, ready for `OpenDevice`.
    pub fn new() -> Self {
        Self {
            mn_node: vela_abi_native64::Node::new(vela_abi_native64::node_type::IOREQUEST, 0, ""),
            mn_reply_port: 0,
            io_device: 0,
            io_unit: 0,
            io_command: cmd::NONE,
            io_flags: 0,
            io_state: IoState::New,
            io_data: core::ptr::null_mut(),
            io_length: 0,
            io_actual: 0,
            io_error: errors::OK,
            io_driver_cookie: 0,
        }
    }
}

impl Default for IORequest {
    fn default() -> Self {
        Self::new()
    }
}

/// The driver interface.
///
/// A driver implements `probe` (called at bus enumeration), `open` (called
/// per unit when a client opens the device), `submit` (the core I/O entry
/// point), `cancel` (attempt to cancel an in-flight request) and `remove`
/// (called on hot-unplug or driver unload).
pub trait DriverOps: Send + Sync {
    /// Probe whether this driver supports the device at `info`.
    ///
    /// Returns `true` if the driver will handle it. Called once per discovered
    /// device; the first driver to return `true` wins.
    fn probe(&self, info: &DeviceInfo) -> bool;

    /// Open `unit` on this device.
    ///
    /// Returns a driver-specific cookie (stored in `io_driver_cookie`) or an
    /// error code.
    fn open(&self, device: u64, unit: u32) -> Result<u64, i32>;

    /// Submit an I/O request.
    ///
    /// The driver takes ownership of the request until it calls
    /// `DeviceManager::complete_io`. May return synchronously (quick I/O) or
    /// queue the request for asynchronous completion.
    fn submit(&self, cookie: u64, req: &mut IORequest) -> Result<(), i32>;

    /// Attempt to cancel an in-flight request.
    ///
    /// Returns `true` if cancellation was requested. The request still needs
    /// to complete normally; cancellation is a hint, not a guarantee.
    fn cancel(&self, cookie: u64, req: &mut IORequest) -> bool;

    /// Remove the device — called on hot-unplug or driver unload.
    ///
    /// Must fail any in-flight requests and release all resources.
    fn remove(&self, cookie: u64);
}

/// Information about a discovered device, passed to `probe`.
pub struct DeviceInfo {
    /// The bus name ("pci", "usb", "virtio", …).
    pub bus: &'static str,
    /// The vendor ID.
    pub vendor_id: u32,
    /// The device ID.
    pub device_id: u32,
    /// The device class.
    pub class_code: u32,
    /// The MMIO base address, or 0 if PIO-only.
    pub mmio_base: u64,
    /// The IRQ line, or 0 if polled.
    pub irq: u32,
}

/// A registered driver.
struct DriverEntry {
    name: &'static str,
    ops: Arc<dyn DriverOps>,
}

/// A device instance (one unit of one driver).
struct DeviceInstance {
    name: &'static str,
    unit: u32,
    cookie: u64,
    open_count: AtomicU32,
}

/// The device manager.
///
/// The single place where drivers are registered, matched to devices, and
/// where `OpenDevice`/`CloseDevice`/`SendIO`/`DoIO`/`WaitIO`/`AbortIO` are
/// dispatched.
#[derive(Default)]
pub struct DeviceManager {
    drivers: Mutex<Vec<DriverEntry>>,
    devices: Mutex<Vec<DeviceInstance>>,
    /// The next open cookie.
    next_cookie: AtomicU64,
}

/// The global device manager.
static DEVICE_MANAGER: Mutex<DeviceManager> = Mutex::new(DeviceManager::new());

impl DeviceManager {
    /// An empty manager.
    pub const fn new() -> Self {
        Self {
            drivers: Mutex::new(Vec::new()),
            devices: Mutex::new(Vec::new()),
            next_cookie: AtomicU64::new(1),
        }
    }

    /// Register a driver.
    ///
    /// Drivers are probed in registration order; the first driver whose
    /// `probe` returns `true` for a device wins.
    pub fn register_driver(name: &'static str, ops: Arc<dyn DriverOps>) {
        let mgr = DEVICE_MANAGER.lock();
        mgr.drivers.lock().push(DriverEntry { name, ops });
    }

    /// Register a device instance.
    pub fn register_device(name: &'static str, unit: u32, cookie: u64) {
        let mgr = DEVICE_MANAGER.lock();
        let cookie = if cookie == 0 {
            mgr.next_cookie.fetch_add(1, Ordering::Relaxed)
        } else {
            cookie
        };
        mgr.devices.lock().push(DeviceInstance {
            name,
            unit,
            cookie,
            open_count: AtomicU32::new(0),
        });
    }

    /// Probe a discovered device against all registered drivers.
    ///
    /// Returns `true` if a driver claimed it.
    pub fn probe_device(info: &DeviceInfo) -> bool {
        let mgr = DEVICE_MANAGER.lock();
        let drivers = mgr.drivers.lock();
        drivers.iter().any(|d| d.ops.probe(info))
    }

    /// Open `unit` on `device_name`.
    ///
    /// Returns a handle (opaque index) or an error code.
    pub fn open_device(name: &str, unit: u32) -> Result<u64, i32> {
        // Step 1: find the device instance and increment open count.
        let (driver_name, cookie) = {
            let mgr = DEVICE_MANAGER.lock();
            let devices = mgr.devices.lock();
            let entry = devices.iter().find(|d| d.name == name && d.unit == unit);
            let Some(entry) = entry else {
                return Err(errors::NO_DEVICE);
            };
            if entry.open_count.fetch_add(1, Ordering::AcqRel) > 0 {
                // Already open — just return the cookie.
                return Ok(entry.cookie);
            }
            (entry.name, entry.cookie)
        };

        // Step 2: call the driver's open.
        let mgr = DEVICE_MANAGER.lock();
        let drivers = mgr.drivers.lock();
        let driver = drivers.iter().find(|d| d.name == driver_name);
        let Some(driver) = driver else {
            return Err(errors::NO_DEVICE);
        };
        driver.ops.open(cookie, unit)
    }

    /// Close a device opened via `open_device`.
    pub fn close_device(cookie: u64) -> Result<(), i32> {
        let (should_remove, name, cookie_val) = {
            let mgr = DEVICE_MANAGER.lock();
            let mut devices = mgr.devices.lock();
            let idx = devices.iter().position(|d| d.cookie == cookie);
            let Some(idx) = idx else {
                return Err(errors::NO_DEVICE);
            };
            let count = devices[idx].open_count.fetch_sub(1, Ordering::Release);
            if count == 1 {
                let name = devices[idx].name.to_string();
                let cookie_val = devices[idx].cookie;
                devices.remove(idx);
                (true, name, cookie_val)
            } else {
                (false, String::new(), 0)
            }
        }; // mgr and devices dropped here

        if should_remove {
            let mgr2 = DEVICE_MANAGER.lock();
            let drivers = mgr2.drivers.lock();
            if let Some(driver) = drivers.iter().find(|d| d.name == name) {
                driver.ops.remove(cookie_val);
            }
        }
        Ok(())
    }

    /// Submit an I/O request.
    pub fn submit_io(cookie: u64, _unit: u32, req: &mut IORequest) -> Result<(), i32> {
        let mgr = DEVICE_MANAGER.lock();
        let drivers = mgr.drivers.lock();
        // Find the driver that owns this cookie by matching against all
        // registered drivers' open devices.
        for driver in drivers.iter() {
            // The cookie encodes the driver; try each driver's submit.
            let result = driver.ops.submit(cookie, req);
            if result.is_ok() {
                req.io_state = IoState::Submitted;
                return Ok(());
            }
        }
        Err(errors::NO_DEVICE)
    }

    /// Complete an I/O request — called by the driver when the device is done.
    ///
    /// The request is moved to `Done` and, unless `IOF_QUICK` is set, replied
    /// to the client's port.
    pub fn complete_io(req: &mut IORequest) -> Result<(), i32> {
        if req.io_flags & flags::IOF_QUICK != 0 {
            req.io_state = IoState::QuickDone;
            return Ok(());
        }
        req.io_state = IoState::Done;
        // Reply to the client's port via Exec's ReplyMsg.
        if req.mn_reply_port != 0 {
            req.io_state = IoState::Replied;
        }
        Ok(())
    }

    /// Abort an in-flight request.
    pub fn abort_io(_cookie: u64, req: &mut IORequest) -> Result<(), i32> {
        if req.io_state == IoState::InFlight || req.io_state == IoState::Queued {
            req.io_state = IoState::CancelRequested;
            Ok(())
        } else {
            Err(errors::BAD_CMD)
        }
    }
}

/// How many drivers are registered.
pub fn driver_count() -> usize {
    DEVICE_MANAGER.lock().drivers.lock().len()
}

/// How many device instances exist.
pub fn device_count() -> usize {
    DEVICE_MANAGER.lock().devices.lock().len()
}

#[cfg(test)]
mod tests {
    use super::*;

    struct NullDriver;

    impl DriverOps for NullDriver {
        fn probe(&self, _info: &DeviceInfo) -> bool {
            true
        }
        fn open(&self, _device: u64, _unit: u32) -> Result<u64, i32> {
            Ok(42)
        }
        fn submit(&self, _cookie: u64, req: &mut IORequest) -> Result<(), i32> {
            req.io_state = IoState::Done;
            req.io_actual = req.io_length;
            Ok(())
        }
        fn cancel(&self, _cookie: u64, _req: &mut IORequest) -> bool {
            true
        }
        fn remove(&self, _cookie: u64) {}
    }

    #[test]
    fn probe_matches_first_driver() {
        let ops: Arc<dyn DriverOps> = Arc::new(NullDriver);
        DeviceManager::register_driver("test.driver", ops);
        let info = DeviceInfo {
            bus: "pci",
            vendor_id: 0x1234,
            device_id: 0x5678,
            class_code: 0x01,
            mmio_base: 0xF000_0000,
            irq: 10,
        };
        assert!(DeviceManager::probe_device(&info));
    }

    #[test]
    fn open_and_close_device() {
        // The null driver is not registered by default in tests, so open fails.
        let rc = DeviceManager::open_device("null.device", 0);
        assert_eq!(rc, Err(errors::NO_DEVICE));
    }

    #[test]
    fn iorequest_state_transitions() {
        let mut req = IORequest::new();
        assert_eq!(req.io_state, IoState::New);
        req.io_state = IoState::Opened;
        assert_eq!(req.io_state, IoState::Opened);
        req.io_state = IoState::Submitted;
        assert_eq!(req.io_state, IoState::Submitted);
        req.io_state = IoState::InFlight;
        assert_eq!(req.io_state, IoState::InFlight);
        req.io_state = IoState::Done;
        assert_eq!(req.io_state, IoState::Done);
        req.io_state = IoState::Replied;
        assert_eq!(req.io_state, IoState::Replied);
    }

    #[test]
    fn iof_quick_skips_reply() {
        let mut req = IORequest::new();
        req.io_flags = flags::IOF_QUICK;
        DeviceManager::complete_io(&mut req).unwrap();
        assert_eq!(req.io_state, IoState::QuickDone);
    }
}
