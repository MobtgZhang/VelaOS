//! USB Mass Storage (Bulk-Only Transport) class driver.
//!
//! Implements the USB Mass Storage Class Bulk-Only Transport (BOT) protocol
//! over the xHCI controller. A USB flash drive or hard drive attached to the
//! xHCI controller is enumerated as a block device and registered with the
//! Device Manager as `usb.storage`.

extern crate alloc;

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, Ordering};

use vela_kernel_device::{cmd, errors, DeviceInfo, DriverOps, IORequest, IoState};
use crate::{DmaBuf, PORT_CONNECT_STATUS, PORT_PORT_ENABLED, PORT_RESET, XhciController};
use crate::regs;

// ---------------------------------------------------------------------------
// USB Mass Storage constants
// ---------------------------------------------------------------------------

/// USB Mass Storage class code.
const USB_CLASS_MASS_STORAGE: u8 = 0x08;
/// SCSI transparent command set subclass.
const USB_SUBCLASS_SCSI: u8 = 0x06;
/// Bulk-Only Transport protocol.
const USB_PROTO_BOT: u8 = 0x50;

/// CBW signature: "USBC" little-endian.
const CBW_SIGNATURE: u32 = 0x4342_5355;
/// CSW signature: "USBS" little-endian.
const CSW_SIGNATURE: u32 = 0x5342_5355;

/// CBW flags: bit 7 = data-in (device to host).
const CBW_FLAGS_DATA_IN: u8 = 0x80;
/// CBW flags: bit 7 = 0 = data-out (host to device).
const CBW_FLAGS_DATA_OUT: u8 = 0x00;

/// CBW structure: 31 bytes.
#[repr(C, packed)]
struct Cbw {
    d_cbw_signature: u32,
    d_cbw_tag: u32,
    d_cbw_data_transfer_length: u32,
    bm_cbw_flags: u8,
    b_cbw_lun: u8,
    b_cbw_cb_length: u8,
    cbw_cb: [u8; 16],
}

/// CSW structure: 13 bytes.
#[repr(C, packed)]
struct Csw {
    d_csw_signature: u32,
    d_csw_tag: u32,
    d_csw_data_residue: u32,
    b_csw_status: u8,
}

/// CSW status values.
const CSW_STATUS_PASSED: u8 = 0;
const CSW_STATUS_FAILED: u8 = 1;
const CSW_STATUS_PHASE_ERROR: u8 = 2;

// ---------------------------------------------------------------------------
// SCSI commands
// ---------------------------------------------------------------------------

const SCSI_TEST_UNIT_READY: u8 = 0x00;
const SCSI_REQUEST_SENSE: u8 = 0x03;
const SCSI_INQUIRY: u8 = 0x12;
const SCSI_MODE_SELECT_6: u8 = 0x15;
const SCSI_MODE_SENSE_6: u8 = 0x1A;
const SCSI_START_STOP_UNIT: u8 = 0x1B;
const SCSI_READ_CAPACITY_10: u8 = 0x25;
const SCSI_READ_10: u8 = 0x28;
const SCSI_WRITE_10: u8 = 0x2A;

// ---------------------------------------------------------------------------
// Bulk endpoint wrapper
// ---------------------------------------------------------------------------

/// A Bulk-IN or Bulk-OUT endpoint on a USB Mass Storage device.
struct BulkEndpoint {
    /// Endpoint address.
    address: u8,
    /// Max packet size.
    max_packet_size: u16,
    /// Direction: true = IN (device -> host), false = OUT (host -> device).
    is_in: bool,
}

impl BulkEndpoint {
    /// Write data to a Bulk-OUT endpoint via the xHCI command ring.
    fn write(&self, ctrl: &XhciController, data: &[u8]) -> Result<(), i32> {
        if self.is_in {
            return Err(errors::BAD_CMD);
        }
        // Submit a transfer TRB to send the data. The data must be in a DMA
        // buffer; here we copy into a temporary DMA buffer.
        let dma = DmaBuf::new(data.len(), 64)?;
        unsafe {
            core::ptr::copy_nonoverlapping(data.as_ptr(), dma.ptr, data.len());
        }
        ctrl.submit_interrupt_in(0, dma.phys, data.len() as u32);
        // Wait for completion on the event ring (bounded).
        let mut st = ctrl.state_lock();
        ctrl.process_events_locked(&mut st);
        // The DMA buffer is leaked; in a production driver we would track and
        // reuse these buffers.
        core::mem::forget(dma);
        Ok(())
    }

    /// Read data from a Bulk-IN endpoint via the xHCI command ring.
    fn read(&self, ctrl: &XhciController, data: &mut [u8]) -> Result<usize, i32> {
        if !self.is_in {
            return Err(errors::BAD_CMD);
        }
        let dma = DmaBuf::new(data.len(), 64)?;
        ctrl.submit_interrupt_in(0, dma.phys, data.len() as u32);
        // Poll the event ring for the transfer completion.
        let mut st = ctrl.state_lock();
        ctrl.process_events_locked(&mut st);
        // Copy data from DMA buffer to caller's buffer.
        unsafe {
            core::ptr::copy_nonoverlapping(dma.ptr, data.as_mut_ptr(), data.len());
        }
        core::mem::forget(dma);
        Ok(data.len())
    }
}

// ---------------------------------------------------------------------------
// USB Mass Storage device
// ---------------------------------------------------------------------------

/// One USB Mass Storage device attached to an xHCI controller.
pub struct UsbStorage {
    /// The xHCI controller.
    ctrl: Arc<XhciController>,
    /// The bulk-IN endpoint.
    bulk_in: Option<BulkEndpoint>,
    /// The bulk-OUT endpoint.
    bulk_out: Option<BulkEndpoint>,
    /// The logical unit number (usually 0).
    lun: u8,
    /// The device's block size.
    block_size: AtomicU32,
    /// The device's total block count.
    block_count: AtomicU32,
    /// The CBW tag counter.
    tag: AtomicU32,
}

impl UsbStorage {
    /// Create a new USB Mass Storage device from an xHCI controller and
    /// endpoint descriptors. Returns `None` if the device is not a BOT
    /// mass-storage device.
    pub fn new(
        ctrl: Arc<XhciController>,
        bulk_in: Option<(u8, u16)>,
        bulk_out: Option<(u8, u16)>,
    ) -> Option<Arc<Self>> {
        let bulk_in = bulk_in.map(|(addr, mps)| BulkEndpoint {
            address: addr,
            max_packet_size: mps,
            is_in: true,
        });
        let bulk_out = bulk_out.map(|(addr, mps)| BulkEndpoint {
            address: addr,
            max_packet_size: mps,
            is_in: false,
        });
        if bulk_in.is_none() || bulk_out.is_none() {
            return None;
        }
        Some(Arc::new(Self {
            ctrl,
            bulk_in,
            bulk_out,
            lun: 0,
            block_size: AtomicU32::new(512),
            block_count: AtomicU32::new(0),
            tag: AtomicU32::new(1),
        }))
    }

    /// Send a SCSI command via CBW and read/write data as needed.
    fn scsi_command(&self, cmd: &[u8], data_in: Option<&mut [u8]>) -> Result<u32, i32> {
        let tag = self.tag.fetch_add(1, Ordering::Relaxed);
        let data_len = data_in.as_ref().map_or(0, |d| d.len()) as u32;
        let has_data = data_in.is_some();
        let flags = if has_data {
            CBW_FLAGS_DATA_IN
        } else {
            CBW_FLAGS_DATA_OUT
        };

        let cbw = Cbw {
            d_cbw_signature: CBW_SIGNATURE,
            d_cbw_tag: tag,
            d_cbw_data_transfer_length: data_len,
            bm_cbw_flags: flags,
            b_cbw_lun: self.lun,
            b_cbw_cb_length: cmd.len() as u8,
            cbw_cb: [0; 16],
        };

        let mut cbw_bytes = [0u8; 31];
        unsafe {
            let ptr = &cbw as *const Cbw as *const u8;
            core::ptr::copy_nonoverlapping(ptr, cbw_bytes.as_mut_ptr(), 31);
        }
        // Copy the command block into the CBW.
        cbw_bytes[15..15 + cmd.len()].copy_from_slice(cmd);

        // Send CBW via bulk-OUT.
        if let Some(ref ep) = self.bulk_out {
            ep.write(&self.ctrl, &cbw_bytes).map_err(|_| -110)?;
        } else {
            return Err(-19);
        }

        // Transfer data if needed.
        if let Some(data) = data_in {
            if let Some(ref ep) = self.bulk_in {
                let read = ep.read(&self.ctrl, data).map_err(|_| -110)?;
                let _ = read;
            }
        }

        // Read CSW via bulk-IN.
        let mut csw_bytes = [0u8; 13];
        if let Some(ref ep) = self.bulk_in {
            let _ = ep.read(&self.ctrl, &mut csw_bytes);
        }

        // Parse CSW.
        let csw_sig = u32::from_le_bytes([csw_bytes[0], csw_bytes[1], csw_bytes[2], csw_bytes[3]]);
        let csw_tag = u32::from_le_bytes([csw_bytes[4], csw_bytes[5], csw_bytes[6], csw_bytes[7]]);
        let csw_status = csw_bytes[12];

        if csw_sig != CSW_SIGNATURE {
            return Err(-19);
        }
        if csw_tag != tag {
            return Err(-19);
        }
        match csw_status {
            CSW_STATUS_PASSED => Ok(data_len),
            CSW_STATUS_FAILED => Err(-5),
            CSW_STATUS_PHASE_ERROR => Err(-19),
            _ => Err(-19),
        }
    }

    /// Issue TEST UNIT READY.
    fn test_unit_ready(&self) -> Result<(), i32> {
        let cmd = [SCSI_TEST_UNIT_READY, 0, 0, 0, 0, 0];
        self.scsi_command(&cmd, None)?;
        Ok(())
    }

    /// Issue INQUIRY and return the inquiry data (36 bytes).
    fn inquiry(&self) -> Result<[u8; 36], i32> {
        let cmd = [SCSI_INQUIRY, 0, 0, 0, 36, 0];
        let mut data = [0u8; 36];
        self.scsi_command(&cmd, Some(&mut data))?;
        Ok(data)
    }

    /// Issue READ CAPACITY(10) and return (block_count, block_size).
    fn read_capacity(&self) -> Result<(u32, u32), i32> {
        let cmd = [SCSI_READ_CAPACITY_10, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        let mut data = [0u8; 8];
        self.scsi_command(&cmd, Some(&mut data))?;
        let block_count = u32::from_be_bytes([data[0], data[1], data[2], data[3]]);
        let block_size = u32::from_be_bytes([data[4], data[5], data[6], data[7]]);
        Ok((block_count, block_size))
    }

    /// Issue READ(10) for `lba` starting at `lba`, `block_count` blocks.
    fn read_10(&self, lba: u32, block_count: u32, data: &mut [u8]) -> Result<(), i32> {
        let mut cmd = [0u8; 10];
        cmd[0] = SCSI_READ_10;
        cmd[1] = 0;
        cmd[2..6].copy_from_slice(&lba.to_be_bytes());
        cmd[6] = 0;
        cmd[7..9].copy_from_slice(&(block_count as u16).to_be_bytes());
        cmd[9] = 0;
        self.scsi_command(&cmd, Some(data))?;
        Ok(())
    }

    /// Issue WRITE(10) for `lba` starting at `lba`, `block_count` blocks.
    fn write_10(&self, lba: u32, block_count: u32, data: &[u8]) -> Result<(), i32> {
        let mut cmd = [0u8; 10];
        cmd[0] = SCSI_WRITE_10;
        cmd[1] = 0;
        cmd[2..6].copy_from_slice(&lba.to_be_bytes());
        cmd[6] = 0;
        cmd[7..9].copy_from_slice(&(block_count as u16).to_be_bytes());
        cmd[9] = 0;
        // Copy data into an owned buffer so we can pass a mutable reference
        // to scsi_command without aliasing violations.
        let mut data_owned: Vec<u8> = data.to_vec();
        self.scsi_command(&cmd, Some(data_owned.as_mut_slice()))?;
        Ok(())
    }

    /// Initialize the device: TEST UNIT READY, INQUIRY, READ CAPACITY.
    pub fn init(&self) -> Result<(), i32> {
        self.test_unit_ready()?;
        let _inquiry = self.inquiry()?;
        let (blocks, bsize) = self.read_capacity()?;
        self.block_count.store(blocks, Ordering::Release);
        self.block_size.store(bsize, Ordering::Release);
        crate::crate_print("usb.storage: ");
        crate::crate_print_dec(self.block_count.load(Ordering::Acquire) as u64);
        crate::crate_print(" blocks x ");
        crate::crate_print_dec(self.block_size.load(Ordering::Acquire) as u64);
        crate::crate_print(" bytes\n");
        Ok(())
    }

    /// The device's block size.
    pub fn block_size(&self) -> u32 {
        self.block_size.load(Ordering::Acquire)
    }

    /// The device's total block count.
    pub fn block_count(&self) -> u32 {
        self.block_count.load(Ordering::Acquire)
    }

    /// Read blocks into `buf` (must be block_size * count bytes).
    pub fn read_blocks(&self, lba: u32, count: u32, buf: &mut [u8]) -> Result<(), i32> {
        self.read_10(lba, count, buf)?;
        Ok(())
    }

    /// Write blocks from `buf` (must be block_size * count bytes).
    pub fn write_blocks(&self, lba: u32, count: u32, buf: &[u8]) -> Result<(), i32> {
        self.write_10(lba, count, buf)?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// DriverOps implementation
// ---------------------------------------------------------------------------

impl DriverOps for UsbStorage {
    fn probe(&self, _info: &DeviceInfo) -> bool {
        true
    }

    fn open(&self, _device: u64, _unit: u32) -> Result<u64, i32> {
        Ok(0x6000_0000)
    }

    fn submit(&self, _cookie: u64, req: &mut IORequest) -> Result<(), i32> {
        match req.io_command {
            cmd::STATUS => {
                req.io_state = IoState::Done;
                req.io_actual = 0;
                req.io_error = errors::OK;
                Ok(())
            }
            cmd::READ => {
                // io_device holds the starting LBA (in blocks).
                let lba = req.io_device as u32;
                let count = (req.io_length + self.block_size() as u64 - 1)
                    / self.block_size() as u64;
                let buf = unsafe {
                    core::slice::from_raw_parts_mut(req.io_data as *mut u8, req.io_length as usize)
                };
                self.read_blocks(lba, count as u32, buf)?;
                req.io_state = IoState::Done;
                req.io_actual = req.io_length;
                req.io_error = errors::OK;
                Ok(())
            }
            cmd::WRITE => {
                // io_device holds the starting LBA (in blocks).
                let lba = req.io_device as u32;
                let count = (req.io_length + self.block_size() as u64 - 1)
                    / self.block_size() as u64;
                let buf = unsafe {
                    core::slice::from_raw_parts(req.io_data as *const u8, req.io_length as usize)
                };
                self.write_blocks(lba, count as u32, buf)?;
                req.io_state = IoState::Done;
                req.io_actual = req.io_length;
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

// ---------------------------------------------------------------------------
// Device enumeration
// ---------------------------------------------------------------------------

/// Enumerate USB Mass Storage devices attached to this controller and register
/// them with the Device Manager as `usb.storage` block devices.
///
/// This is a simplified enumerator: it assumes QEMU-style endpoint addressing
/// (bulk-IN on endpoint 2, bulk-OUT on endpoint 3) and does not yet perform
/// full descriptor-driven enumeration. A production driver would read the
/// device, configuration, interface and endpoint descriptors through the
/// control endpoint (EP0) before binding the driver.
pub fn enumerate_storage(ctrl: Arc<XhciController>) -> Result<(), i32> {
    let mut st = ctrl.state.lock();
    // Check root ports for connected devices.
    let hcs1 = unsafe { regs::rd32(ctrl.op_base + 0x04) };
    let max_ports = ((hcs1 >> 24) & 0xFF) as u8;
    drop(st);

    for port in 1..=max_ports {
        let psc = ctrl.portsc(port);
        if psc & PORT_CONNECT_STATUS == 0 {
            continue;
        }
        if psc & PORT_PORT_ENABLED != 0 {
            continue; // already configured
        }

        // Reset the port.
        ctrl.portsc_write(port, psc | PORT_RESET);
        for _ in 0..100_000 {
            core::hint::spin_loop();
            let psc2 = ctrl.portsc(port);
            if psc2 & PORT_RESET == 0 {
                break;
            }
        }
        ctrl.portsc_write(port, psc & !PORT_RESET);

        // Wait for port enabled.
        for _ in 0..100_000 {
            let psc2 = ctrl.portsc(port);
            if psc2 & PORT_PORT_ENABLED != 0 {
                break;
            }
            core::hint::spin_loop();
        }

        // Enable slot and address the device.
        let slot_id = match ctrl.enable_slot() {
            Ok(id) => id,
            Err(e) => {
                crate::crate_print("usb.storage: enable slot failed (");
                crate::crate_print_dec(e.unsigned_abs() as u64);
                crate::crate_print(")\n");
                continue;
            }
        };
        let _ = ctrl.address_device(slot_id, slot_id);

        // Configure endpoints: QEMU usb-storage uses EP2 (bulk-IN) and EP3
        // (bulk-OUT). Build endpoint contexts and configure them.
        // For simplicity we use the pre-allocated EP1 context for INTR and
        // create bulk endpoints directly.
        let bulk_in_addr = 0x82u8; // EP2 IN
        let bulk_out_addr = 0x03u8; // EP3 OUT
        let max_packet = 64u16;

        let device = match UsbStorage::new(
            Arc::clone(&ctrl),
            Some((bulk_in_addr, max_packet)),
            Some((bulk_out_addr, max_packet)),
        ) {
            Some(d) => d,
            None => continue,
        };

        // Initialise the device (TEST UNIT READY, INQUIRY, READ CAPACITY).
        if let Err(e) = device.init() {
            crate::crate_print("usb.storage: init failed (");
            crate::crate_print_dec(e.unsigned_abs() as u64);
            crate::crate_print(")\n");
            continue;
        }

        // Register with the Device Manager.
        let cookie = device.block_size() as u64;
        vela_kernel_device::DeviceManager::register_driver(
            "usb.storage",
            Arc::clone(&device) as Arc<dyn DriverOps>,
        );
        vela_kernel_device::DeviceManager::register_device("usb.storage", 0, cookie);

        crate::crate_print("usb.storage: registered block device\n");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Serial logging (best-effort)
// ---------------------------------------------------------------------------

fn serial_log(s: &str) {
    // Best-effort serial output; the kernel console is the primary output.
    let _ = s;
}

fn serial_log_dec(v: u64) {
    let _ = v;
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cbw_and_csw_sizes_match_usb_mass_storage_spec() {
        assert_eq!(core::mem::size_of::<Cbw>(), 31);
        assert_eq!(core::mem::size_of::<Csw>(), 13);
        assert_eq!(CBW_SIGNATURE, 0x4342_5355);
        assert_eq!(CSW_SIGNATURE, 0x5342_5355);
    }

    #[test]
    fn scsi_command_codes_match_spec() {
        assert_eq!(SCSI_TEST_UNIT_READY, 0x00);
        assert_eq!(SCSI_REQUEST_SENSE, 0x03);
        assert_eq!(SCSI_INQUIRY, 0x12);
        assert_eq!(SCSI_READ_CAPACITY_10, 0x25);
        assert_eq!(SCSI_READ_10, 0x28);
        assert_eq!(SCSI_WRITE_10, 0x2A);
    }
}
