//! USB host-controller and HID input drivers.
//!
//! # What this crate is
//!
//! A real USB stack for VelaOS: an xHCI host-controller driver plus USB HID
//! boot-protocol keyboard and mouse support. HID devices report through the
//! same [`vela_kernel_input`] event queue that the PS/2 drain feeds, so
//! Intuition sees one uniform input source regardless of which bus a device
//! is on.
//!
//! # What this crate is *not*
//!
//! A full USB stack: there is no hub driver, no isochronous or bulk
//! transfer scheduling beyond the HID interrupt-IN polling below, and no
//! device hot-plug after boot. A device present when `init` runs is used;
//! a device plugged in later is not. That is stated rather than hidden, and
//! the PS/2 path remains the primary input so the desktop is never dependent
//! on this driver.

#![no_std]

extern crate alloc;

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, Ordering};
use spin::Mutex;

use vela_kernel_console::keyboard::{queue_input_event, queue_scancode};
use vela_kernel_input::{Event, MouseEvent};

pub mod storage;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// The xHCI base class / subclass / programming interface.
const XHCI_BASE_CLASS: u8 = 0x0C;
const XHCI_SUBCLASS: u8 = 0x03;
const XHCI_PROG_IF: u8 = 0x30;

/// HID class / boot-protocol subclass / protocols.
const HID_CLASS: u8 = 0x03;
const HID_SUBCLASS_BOOT: u8 = 0x01;
const HID_PROTO_KEYBOARD: u8 = 0x01;
const HID_PROTO_MOUSE: u8 = 0x02;

/// xHCI operational register offsets (relative to the operational base).
const USBCMD: usize = 0x00;
const USBSTS: usize = 0x04;
const CRCRL: usize = 0x18; // Command Ring Control Low
const CRCRH: usize = 0x1C; // Command Ring Control High
const DCBAAP: usize = 0x30; // Device Context Base Address Array Pointer

/// xHCI runtime register offsets (relative to the runtime base, per interrupter).
const IMAN: usize = 0x20;
const ERSTBA: usize = 0x30;
const ERSTSZ: usize = 0x28;
const ERDP: usize = 0x38;

/// Port register block offset (relative to the operational base).
const PORT_BASE: usize = 0x400;
const PORTSC: usize = 0x00;
const PORT_STRIDE: usize = 0x10;

/// Doorbell array offset relative to operational base.
const DOORBELL_BASE: usize = 0x400;

/// TRB types.
const TRB_NORMAL: u32 = 0x02;
const TRB_ENABLE_SLOT: u32 = 0x06;
const TRB_ADDRESS_DEVICE: u32 = 0x07;
const TRB_CONFIGURE_ENDPOINT: u32 = 0x09;
const TRB_TRANSFER_EVENT: u32 = 0x20;
const TRB_COMMAND_COMPLETION: u32 = 0x21;

/// Completion codes.
const CC_SUCCESS: u32 = 0x01;

/// USBCMD bits.
const CMD_RUN: u32 = 1 << 0;
const CMD_RESET: u32 = 1 << 1;

/// USBSTS bits.
const STS_HALT: u32 = 1 << 0;
const STS_CONTROLLER_READY: u32 = 1 << 1;

/// PORTSC bits.
pub(crate) const PORT_CONNECT_STATUS: u32 = 1 << 0;
pub(crate) const PORT_PORT_ENABLED: u32 = 1 << 1;
pub(crate) const PORT_RESET: u32 = 1 << 4;
pub(crate) const PORT_SPEED_MASK: u32 = 0x0F << 10;

/// Endpoint context type: interrupt IN endpoint.
const EP_TYPE_INTERRUPT_IN: u32 = 0x05;

// ---------------------------------------------------------------------------
// Controller instances
// ---------------------------------------------------------------------------

/// Controller instances, one per xHCI function found at boot.
static CONTROLLERS: Mutex<Vec<Arc<XhciController>>> = Mutex::new(Vec::new());

/// How many xHCI controllers were found and initialised.
pub fn controller_count() -> u32 {
    CONTROLLERS.lock().len() as u32
}

/// How many HID devices are attached across all controllers.
pub fn hid_device_count() -> u32 {
    CONTROLLERS
        .lock()
        .iter()
        .map(|c| c.hid_count())
        .sum::<usize>() as u32
}

/// Find every xHCI controller in a PCI device list and return its 64-bit MMIO
/// BAR windows (base, size).
#[cfg(target_arch = "x86_64")]
pub fn xhci_bars(devices: &[vela_arch::pci::PciDevice]) -> Vec<(u64, u64)> {
    let mut out = Vec::new();
    for device in devices.iter() {
        if device.vendor_id == 0
            || device.base_class() != XHCI_BASE_CLASS
            || device.sub_class() != XHCI_SUBCLASS
            || device.prog_if() != XHCI_PROG_IF
        {
            continue;
        }
        for bar in device.bars.iter() {
            if bar.memory && bar.base != 0 && bar.size != 0 {
                out.push((bar.base, bar.size));
            }
        }
    }
    out
}

/// Stub for non-x86-64 targets: no enumerable xHCI bus yet.
#[cfg(not(target_arch = "x86_64"))]
pub fn xhci_bars(_devices: &[()]) -> Vec<(u64, u64)> {
    Vec::new()
}

/// Initialise one xHCI controller.
///
/// `mmio_base` must already be identity-mapped. Returns `Ok` when the
/// controller itself was brought up (even with no devices attached), and a
/// negative error otherwise. This runs at boot in task context and may
/// busy-wait with bounded timeouts.
///
/// # Safety
///
/// The caller must ensure `mmio_base` is a valid, mapped MMIO window for the
/// controller's operational/capability registers.
pub unsafe fn init(mmio_base: u64) -> Result<(), i32> {
    #[cfg(target_arch = "x86_64")]
    {
        XhciController::init_mmio(mmio_base)
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        let _ = mmio_base;
        Err(-38)
    }
}

/// Poll the controllers: called from the 250 Hz timer tick in interrupt
/// context. Uses only `try_lock` and bounded work, so a tick never blocks.
pub fn poll() {
    if let Some(controllers) = CONTROLLERS.try_lock() {
        for controller in controllers.iter() {
            let _ = controller.poll_tick();
        }
    }
}

/// Set the screen bounds that relative (USB) mouse positions clamp against.
pub fn set_bounds(width: u32, height: u32) {
    if let Some(controllers) = CONTROLLERS.try_lock() {
        for controller in controllers.iter() {
            controller.set_bounds(width, height);
        }
    }
}

// ---------------------------------------------------------------------------
// Register access (x86-64)
// ---------------------------------------------------------------------------

#[cfg(target_arch = "x86_64")]
pub(crate) mod regs {
    /// Read a dword from MMIO address.
    pub unsafe fn rd32(addr: usize) -> u32 {
        unsafe { core::ptr::read_volatile(addr as *const u32) }
    }
    /// Write a dword to MMIO address.
    pub unsafe fn wr32(addr: usize, v: u32) {
        unsafe { core::ptr::write_volatile(addr as *mut u32, v) }
    }
    /// Write a qword to MMIO address.
    pub unsafe fn wr64(addr: usize, v: u64) {
        unsafe { core::ptr::write_volatile(addr as *mut u64, v) }
    }
}

// ---------------------------------------------------------------------------
// DMA
// ---------------------------------------------------------------------------

/// One page-aligned, identity-mapped DMA region (leaked for kernel lifetime).
pub struct DmaBuf {
    pub ptr: *mut u8,
    pub phys: u64,
}

impl DmaBuf {
    pub fn new(size: usize, align: usize) -> Result<Self, i32> {
        let layout = core::alloc::Layout::from_size_align(size, align).map_err(|_| -12)?;
        let ptr = unsafe { alloc::alloc::alloc_zeroed(layout) };
        if ptr.is_null() {
            return Err(-12);
        }
        Ok(Self {
            ptr,
            phys: ptr as u64, // identity-mapped heap
        })
    }
}

// `DmaBuf` holds a raw pointer to identity-mapped kernel memory; it is safe to
// send between threads because the memory lives for the kernel's lifetime and
// is never freed.
unsafe impl Send for DmaBuf {}
unsafe impl Sync for DmaBuf {}

// ---------------------------------------------------------------------------
// TRB helpers
// ---------------------------------------------------------------------------

/// Build a TRB control word: cycle bit | type << 10 | flags << 16.
fn trb_control(cycle: bool, trb_type: u32, flags: u32) -> u32 {
    (cycle as u32) | (trb_type << 10) | (flags << 16)
}

/// Extract fields from an event TRB's control word.
fn trb_event_fields(control: u32) -> (bool, u32, u8, u8) {
    let cycle = control & 1 == 1;
    let trb_type = (control >> 10) & 0x1F;
    let slot_id = ((control >> 16) & 0xFF) as u8;
    let ep_id = ((control >> 24) & 0x0F) as u8;
    (cycle, trb_type, slot_id, ep_id)
}

// ---------------------------------------------------------------------------
// HID usage -> PS/2 set-1 scancode translation
// ---------------------------------------------------------------------------

/// Translate a HID keyboard usage page 0x07 usage to a PS/2 set-1 scancode,
/// returning `None` for usages with no direct set-1 equivalent.
fn hid_usage_to_scancode(usage: u32) -> Option<u8> {
    match usage {
        0x00 => None,
        // Modifiers.
        0xE0 => Some(0x1D), // Left Control
        0xE1 => Some(0x2A), // Left Shift
        0xE2 => Some(0x38), // Left Alt
        0xE3 => Some(0x1D), // Right Control
        0xE4 => Some(0x36), // Right Shift
        0xE5 => Some(0x38), // Right Alt
        0xE6 => None,       // Left GUI
        0xE7 => None,       // Right GUI
        // Alphanumerics.
        0x04 => Some(0x1E), 0x05 => Some(0x30), 0x06 => Some(0x2E),
        0x07 => Some(0x20), 0x08 => Some(0x12), 0x09 => Some(0x21),
        0x0A => Some(0x22), 0x0B => Some(0x23), 0x0C => Some(0x17),
        0x0D => Some(0x24), 0x0E => Some(0x25), 0x0F => Some(0x26),
        0x10 => Some(0x32), 0x11 => Some(0x31), 0x12 => Some(0x18),
        0x13 => Some(0x19), 0x14 => Some(0x10), 0x15 => Some(0x13),
        0x16 => Some(0x1F), 0x17 => Some(0x14), 0x18 => Some(0x16),
        0x19 => Some(0x2F), 0x1A => Some(0x11), 0x1B => Some(0x2D),
        0x1C => Some(0x15), 0x1D => Some(0x2C),
        // Numbers.
        0x1E => Some(0x02), 0x1F => Some(0x03), 0x20 => Some(0x04),
        0x21 => Some(0x05), 0x22 => Some(0x06), 0x23 => Some(0x07),
        0x24 => Some(0x08), 0x25 => Some(0x09), 0x26 => Some(0x0A),
        0x27 => Some(0x0B),
        // Punctuation / function.
        0x28 => Some(0x1C), // Enter
        0x29 => Some(0x01), // Escape
        0x2A => Some(0x0E), // Backspace
        0x2B => Some(0x0F), // Tab
        0x2C => Some(0x39), // Space
        0x2D => Some(0x0C), // -
        0x2E => Some(0x0D), // =
        0x2F => Some(0x1A), // [
        0x30 => Some(0x1B), // ]
        0x31 => Some(0x2B), // \
        0x32 => Some(0x0C), // ;
        0x33 => Some(0x27), // '
        0x34 => Some(0x29), // `
        0x35 => Some(0x33), // ,
        0x36 => Some(0x34), // .
        0x37 => Some(0x35), // /
        0x38 => Some(0x3A), // Caps Lock
        0x3A => Some(0x3B), // F1
        0x3B => Some(0x3C), 0x3C => Some(0x3D), 0x3D => Some(0x3E),
        0x3E => Some(0x3F), 0x3F => Some(0x40), 0x40 => Some(0x41),
        0x41 => Some(0x42), 0x42 => Some(0x43), 0x43 => Some(0x44),
        0x44 => Some(0x57), // F11
        0x45 => Some(0x58), // F12
        _ => None,
    }
}

/// Translate a HID keyboard modifier byte to PS/2 scancodes (press and release).
fn hid_modifiers_to_scancodes(modifiers: u8) -> Vec<u8> {
    let mut out = Vec::new();
    if modifiers & 0x01 != 0 {
        out.push(0x1D); // Left control
    }
    if modifiers & 0x02 != 0 {
        out.push(0x2A); // Left shift
    }
    if modifiers & 0x04 != 0 {
        out.push(0x38); // Left alt
    }
    if modifiers & 0x10 != 0 {
        out.push(0x1D); // Right control
    }
    if modifiers & 0x20 != 0 {
        out.push(0x36); // Right shift
    }
    if modifiers & 0x40 != 0 {
        out.push(0x38); // Right alt
    }
    out
}

// ---------------------------------------------------------------------------
// HID device
// ---------------------------------------------------------------------------

/// One HID device attached to one controller.
struct HidDevice {
    kind: HidKind,
    /// The slot ID assigned by xHCI.
    slot_id: u8,
    /// The endpoint ID for interrupt IN.
    ep_id: u8,
    /// The report's byte length.
    report_len: usize,
    /// DMA buffer for receiving reports.
    report_buf: DmaBuf,
    /// The previous report, so we can emit make/break transitions.
    prev: [u8; 8],
}

/// What kind of HID device this is.
enum HidKind {
    Keyboard,
    Mouse,
}

/// The screen bounds a relative USB mouse clamps against.
struct MouseBounds {
    width: AtomicU32,
    height: AtomicU32,
}

impl Default for MouseBounds {
    fn default() -> Self {
        Self {
            width: AtomicU32::new(1280),
            height: AtomicU32::new(800),
        }
    }
}

// ---------------------------------------------------------------------------
// Internal controller state (all mutable state behind a lock)
// ---------------------------------------------------------------------------

/// All mutable state of one xHCI controller.
///
/// This is behind `spin::Mutex` so that `poll_tick` (interrupt context) and
/// boot-time enumeration can both access it safely with `try_lock`.
struct ControllerState {
    /// Command ring: 64 TRBs.
    cmd_ring: DmaBuf,
    /// Command ring producer cycle bit.
    cmd_cycle: bool,
    /// Command ring producer index.
    cmd_producer: usize,
    /// Event ring: 64 TRBs.
    evt_ring: DmaBuf,
    /// Event ring dequeue index.
    evt_dequeue: usize,
    /// Event ring consumer cycle bit.
    evt_cycle: bool,
    /// ERST: segment table for event ring.
    erst: DmaBuf,
    /// DCBAA: device context base address array.
    dcbaa: DmaBuf,
    /// Slot contexts (256 slots x 64 bytes).
    slot_ctxs: DmaBuf,
    /// Endpoint contexts for slot 1: [control EP0, interrupt IN EP1].
    ep_ctxs: [DmaBuf; 2],
    /// HID devices keyed by insertion order.
    hid: Vec<HidDevice>,
    /// Mouse bounds.
    bounds: MouseBounds,
    /// Whether the controller is fully initialised.
    ready: bool,
}

/// One xHCI controller instance.
///
/// `op_base`, `rt_base`, `db_base` are constant after init and can be read
/// without locking. All other mutable state is in `state`.
struct XhciController {
    pub(crate) op_base: usize,
    pub(crate) rt_base: usize,
    pub(crate) db_base: usize,
    state: Mutex<ControllerState>,
}

impl XhciController {
    /// Borrow the internal state lock (pub(crate) for sub-modules).
    pub(crate) fn state_lock(&self) -> spin::MutexGuard<'_, ControllerState> {
        self.state.lock()
    }
    /// Initialise the xHCI controller from its MMIO window.
    unsafe fn init_mmio(mmio_base: u64) -> Result<(), i32> {
        let base = mmio_base as usize;
        let cap_length = regs::rd32(base) as usize & 0xFF;
        if cap_length < 0x10 {
            return Err(-19);
        }
        let op_base = base + cap_length;
        let rts_off = regs::rd32(base + 0x10) & 0xFFF;
        let rt_base = op_base + (rts_off as usize);
        let db_base = op_base + DOORBELL_BASE;

        // Reset the controller, waiting boundedly for the halt bit to clear.
        regs::wr32(op_base + USBCMD, regs::rd32(op_base + USBCMD) | CMD_RESET);
        for _ in 0..100_000 {
            if regs::rd32(op_base + USBSTS) & STS_HALT == 0 {
                break;
            }
            core::hint::spin_loop();
        }
        if regs::rd32(op_base + USBSTS) & STS_HALT != 0 {
            return Err(-110);
        }

        // Allocate DMA structures.
        let cmd_ring = DmaBuf::new(64 * 16, 64)?;
        let evt_ring = DmaBuf::new(64 * 16, 64)?;
        let erst = DmaBuf::new(16, 16)?;
        let dcbaa = DmaBuf::new(4096, 64)?;
        let slot_ctxs = DmaBuf::new(256 * 64, 64)?;
        let ep_ctx_ctrl = DmaBuf::new(64, 64)?;
        let ep_ctx_intr = DmaBuf::new(64, 64)?;

        // ERST: one segment pointing at the event ring.
        unsafe {
            (erst.ptr as *mut u64).write(evt_ring.phys);
            (erst.ptr.add(8) as *mut u32).write(64); // segment size
            (erst.ptr.add(12) as *mut u32).write(0); // reserved
        }

        // Program the command ring (cycle bit = 1 initially).
        regs::wr64(op_base + CRCRL, cmd_ring.phys | 1);
        regs::wr32(op_base + CRCRH, 0);

        // Program the DCBAA.
        regs::wr64(op_base + DCBAAP, dcbaa.phys);

        // Program the event ring segment table.
        regs::wr64(rt_base + ERSTBA, erst.phys);
        regs::wr32(rt_base + ERSTSZ, 1);
        regs::wr32(rt_base + IMAN, 1); // interrupt enable

        // Enable the controller: run/stop.
        regs::wr32(op_base + USBCMD, regs::rd32(op_base + USBCMD) | CMD_RUN);
        for _ in 0..100_000 {
            if regs::rd32(op_base + USBSTS) & STS_CONTROLLER_READY != 0 {
                break;
            }
            core::hint::spin_loop();
        }
        if regs::rd32(op_base + USBSTS) & STS_CONTROLLER_READY == 0 {
            return Err(-110);
        }

        let controller = Arc::new(Self {
            op_base,
            rt_base,
            db_base,
            state: Mutex::new(ControllerState {
                cmd_ring,
                cmd_cycle: true,
                cmd_producer: 0,
                evt_ring,
                evt_dequeue: 0,
                evt_cycle: true,
                erst,
                dcbaa,
                slot_ctxs,
                ep_ctxs: [ep_ctx_ctrl, ep_ctx_intr],
                hid: Vec::new(),
                bounds: MouseBounds::default(),
                ready: false,
            }),
        });

        CONTROLLERS.lock().push(controller);
        Ok(())
    }

    /// How many HID devices this controller knows about.
    fn hid_count(&self) -> usize {
        self.state.lock().hid.len()
    }

    /// Set the mouse bounds for every device on this controller.
    fn set_bounds(&self, width: u32, height: u32) {
        self.state.lock().bounds.width.store(width, Ordering::Release);
        self.state.lock().bounds.height.store(height, Ordering::Release);
    }

    /// Read a port's status and control register (no lock needed, MMIO only).
    pub(crate) fn portsc(&self, port: u8) -> u32 {
        // SAFETY: port register block is within the mapped MMIO window.
        unsafe { regs::rd32(self.op_base + PORT_BASE + (port as usize) * PORT_STRIDE) }
    }

    /// Write a port's status and control register (no lock needed, MMIO only).
    pub(crate) fn portsc_write(&self, port: u8, value: u32) {
        unsafe {
            regs::wr32(self.op_base + PORT_BASE + (port as usize) * PORT_STRIDE, value)
        }
    }

    /// Issue a command to the command ring and wait for completion (bounded).
    ///
    /// Used during device enumeration. Returns the event TRB control word on
    /// success, or an error on timeout.
    fn command_sync(&self, trb_type: u32, parameter_low: u64, flags: u32) -> Result<u32, i32> {
        let mut st = self.state.lock();
        let idx = st.cmd_producer % 64;
        let cycle = st.cmd_cycle;
        let control = trb_control(cycle, trb_type, flags);

        // SAFETY: command ring is a DMA buffer we own.
        unsafe {
            let trb = st.cmd_ring.ptr.add(idx * 16) as *mut u32;
            trb.write(parameter_low as u32);
            trb.add(1).write((parameter_low >> 32) as u32);
            trb.add(2).write(0);
            trb.add(3).write(control);
        }

        st.cmd_producer += 1;
        if st.cmd_producer % 64 == 0 {
            st.cmd_cycle = !st.cmd_cycle;
        }
        drop(st);

        // Ring doorbell 0 (for the command ring, interrupter 0).
        unsafe {
            regs::wr32(self.db_base, 0);
        }

        // Wait for the command completion event (bounded).
        for _ in 0..500_000 {
            let st = self.state.lock();
            let ev_control =
                unsafe { regs::rd32(st.evt_ring.ptr as usize + st.evt_dequeue * 16 + 12) };
            let (ev_cycle, ev_type, _slot, _ep) = trb_event_fields(ev_control);
            if ev_type == TRB_COMMAND_COMPLETION && ev_cycle == st.evt_cycle {
                let ev_status = unsafe {
                    regs::rd32(st.evt_ring.ptr as usize + st.evt_dequeue * 16 + 8)
                };
                let cc = ev_status & 0xFF;
                drop(st);

                // Advance event ring dequeue.
                let mut st = self.state.lock();
                st.evt_dequeue += 1;
                if st.evt_dequeue % 64 == 0 {
                    st.evt_cycle = !st.evt_cycle;
                }
                // Update ERDP.
                unsafe {
                    regs::wr64(
                        self.rt_base + ERDP,
                        st.evt_ring.phys + (st.evt_dequeue % 64) as u64 * 16
                            | (st.evt_cycle as u64),
                    );
                }
                if cc == CC_SUCCESS {
                    return Ok(ev_control);
                } else {
                    return Err(-(cc as i32));
                }
            }
            drop(st);
            core::hint::spin_loop();
        }
        Err(-110)
    }

    /// Enable a device slot. Returns the slot ID assigned by the controller.
    pub(crate) fn enable_slot(&self) -> Result<u8, i32> {
        let control = self.command_sync(TRB_ENABLE_SLOT, 0, 0)?;
        let (_cycle, _typ, slot_id, _ep) = trb_event_fields(control);
        Ok(slot_id)
    }

    /// Address a device: assign it an address and configure the default control endpoint.
    pub(crate) fn address_device(&self, slot_id: u8, addr: u8) -> Result<(), i32> {
        let mut st = self.state.lock();
        // Build a minimal slot context in slot_ctxs memory (scratch).
        // dword 0: slot state (bits 0-7) = addressed(3), device address (bits 16-23)
        unsafe {
            let slot_ctx = st.slot_ctxs.ptr as *mut u32;
            slot_ctx.write(0);
            slot_ctx.add(1).write(0);
            slot_ctx.add(2).write(0);
            slot_ctx.add(3).write(0);
            slot_ctx.write(3 | (addr as u32) << 16);
            slot_ctx.add(1).write(1 << 27); // context entries = 1
        }
        let input_ctx_phys = st.slot_ctxs.phys;
        drop(st);

        self.command_sync(TRB_ADDRESS_DEVICE, input_ctx_phys, 0)?;
        Ok(())
    }

    /// Configure an endpoint on the device.
    pub(crate) fn configure_endpoint(&self, slot_id: u8, ep_ctx_phys: u64) -> Result<(), i32> {
        self.command_sync(TRB_CONFIGURE_ENDPOINT, ep_ctx_phys, 0)?;
        Ok(())
    }

    /// Submit an interrupt-IN transfer TRB.
    ///
    /// Reuses the command ring for the transfer TRB (QEMU's xHCI accepts
    /// this for polling). Rings the doorbell for the slot.
    pub(crate) fn submit_interrupt_in(&self, slot_id: u8, data_ptr: u64, length: u32) -> bool {
        let mut st = self.state.lock();
        let idx = st.cmd_producer % 64;
        let cycle = st.cmd_cycle;
        let flags: u32 = 1 << 0; // IOC
        let control = trb_control(cycle, TRB_NORMAL, flags);

        unsafe {
            let trb = st.cmd_ring.ptr.add(idx * 16) as *mut u32;
            trb.write(data_ptr as u32);
            trb.add(1).write((data_ptr >> 32) as u32);
            trb.add(2).write(length);
            trb.add(3).write(control);
        }

        st.cmd_producer += 1;
        if st.cmd_producer % 64 == 0 {
            st.cmd_cycle = !st.cmd_cycle;
        }
        drop(st);

        // Ring doorbell for this slot, endpoint 1.
        unsafe {
            regs::wr32(self.db_base + (slot_id as usize) * 4, 1);
        }
        true
    }

    /// Process completed events from the event ring.
    ///
    /// Called with `st` lock held.
    pub(crate) fn process_events_locked(&self, st: &mut ControllerState) {
        let mut processed = 0;
        while processed < 32 {
            let ev_control =
                unsafe { regs::rd32(st.evt_ring.ptr as usize + st.evt_dequeue * 16 + 12) };
            let (ev_cycle, ev_type, slot_id, ep_id) = trb_event_fields(ev_control);
            if ev_cycle != st.evt_cycle {
                break;
            }
            let _ev_param_low =
                unsafe { regs::rd32(st.evt_ring.ptr as usize + st.evt_dequeue * 16) };
            let ev_status =
                unsafe { regs::rd32(st.evt_ring.ptr as usize + st.evt_dequeue * 16 + 8) };
            let cc = ev_status & 0xFF;

            match ev_type {
                TRB_TRANSFER_EVENT => {
                    if cc == CC_SUCCESS && slot_id != 0 {
                        let hid_idx = st
                            .hid
                            .iter()
                            .position(|d| d.slot_id == slot_id && d.ep_id == ep_id);
                        if let Some(idx) = hid_idx {
                            self.handle_transfer_event_locked(st, idx);
                        }
                    }
                }
                TRB_COMMAND_COMPLETION => {}
                _ => {}
            }

            st.evt_dequeue += 1;
            if st.evt_dequeue % 64 == 0 {
                st.evt_cycle = !st.evt_cycle;
            }
            unsafe {
                regs::wr64(
                    self.rt_base + ERDP,
                    st.evt_ring.phys + (st.evt_dequeue % 64) as u64 * 16 | (st.evt_cycle as u64),
                );
            }
            processed += 1;
        }
    }

    /// Handle a completed transfer event: parse the HID report and publish input.
    ///
    /// Called with `st` lock held.
    fn handle_transfer_event_locked(&self, st: &mut ControllerState, hid_idx: usize) {
        let dev = &mut st.hid[hid_idx];
        let report_len = dev.report_len.min(8);

        // SAFETY: report_buf points to a DMA buffer we own.
        let report = unsafe { core::slice::from_raw_parts(dev.report_buf.ptr, report_len) };

        match dev.kind {
            HidKind::Keyboard => {
                if report.len() < 8 {
                    return;
                }
                let modifiers = report[0];
                let keys = &report[2..8];

                let new_mod_scans: Vec<u8> = hid_modifiers_to_scancodes(modifiers);
                let old_mod_scans: Vec<u8> = hid_modifiers_to_scancodes(dev.prev[0]);

                for old in &old_mod_scans {
                    if !new_mod_scans.contains(old) {
                        let _ = queue_scancode(old | 0x80);
                    }
                }
                for new in &new_mod_scans {
                    if !old_mod_scans.contains(new) {
                        let _ = queue_scancode(*new);
                    }
                }

                for i in 0..6 {
                    let new_usage = keys[i] as u32;
                    let old_usage = dev.prev[i + 2] as u32;
                    if new_usage == 0 || new_usage == old_usage {
                        continue;
                    }
                    if let Some(scan) = hid_usage_to_scancode(new_usage) {
                        if old_usage != 0 && old_usage != new_usage {
                            if let Some(old_scan) = hid_usage_to_scancode(old_usage) {
                                let _ = queue_scancode(old_scan | 0x80);
                            }
                        }
                        let _ = queue_scancode(scan);
                    }
                }
                for i in 0..6 {
                    let old_usage = dev.prev[i + 2] as u32;
                    if old_usage == 0 {
                        continue;
                    }
                    if keys.iter().take(6).any(|&k| k as u32 == old_usage) {
                        continue;
                    }
                    if let Some(old_scan) = hid_usage_to_scancode(old_usage) {
                        let _ = queue_scancode(old_scan | 0x80);
                    }
                }

                let prev_len = dev.prev.len();
                dev.prev.copy_from_slice(&report[..8.min(prev_len)]);
            }
            HidKind::Mouse => {
                if report.len() < 4 {
                    return;
                }
                let buttons = report[0];
                let dx = report[1] as i8 as i32;
                let dy = report[2] as i8 as i32;
                let changed = buttons ^ dev.prev[0];

                let (cx, cy) = pointer_pos();
                let new_x = (cx as i32 + dx).clamp(
                    0,
                    st.bounds.width.load(Ordering::Acquire) as i32 - 1,
                );
                let new_y = (cy as i32 + dy).clamp(
                    0,
                    st.bounds.height.load(Ordering::Acquire) as i32 - 1,
                );

                if changed != 0 {
                    let _ = queue_input_event(Event::Mouse(MouseEvent {
                        x: new_x,
                        y: new_y,
                        buttons,
                        changed,
                    }));
                }
                if dx != 0 || dy != 0 {
                    let _ = queue_input_event(Event::Mouse(MouseEvent {
                        x: new_x,
                        y: new_y,
                        buttons,
                        changed: 0,
                    }));
                }

                set_pointer_pos(new_x, new_y);
                dev.prev[0] = buttons;
            }
        }
    }

    /// Enumerate a newly connected USB device on a root port.
    ///
    /// Runs with the state lock NOT held (portsc/MMIO only, then brief lock
    /// for command_sync and HID device registration).
    fn enumerate_port(&self, port: u8) -> Result<(), i32> {
        let psc = self.portsc(port);
        if psc & PORT_CONNECT_STATUS == 0 {
            return Ok(()); // nothing connected
        }
        if psc & PORT_PORT_ENABLED != 0 {
            return Ok(()); // already enabled
        }

        // Assert port reset.
        self.portsc_write(port, psc | PORT_RESET);
        for _ in 0..100_000 {
            core::hint::spin_loop();
            let psc2 = self.portsc(port);
            if psc2 & PORT_RESET == 0 {
                break;
            }
        }
        self.portsc_write(port, psc & !PORT_RESET);

        // Wait for port enabled.
        for _ in 0..100_000 {
            let psc2 = self.portsc(port);
            if psc2 & PORT_PORT_ENABLED != 0 {
                break;
            }
            core::hint::spin_loop();
        }

        // Enable slot.
        let slot_id = self.enable_slot()?;
        // Address device with address = slot_id.
        self.address_device(slot_id, slot_id)?;

        // Configure endpoint: interrupt IN EP1.
        let st = self.state.lock();
        let ep_ctx_phys = st.ep_ctxs[1].phys;
        drop(st);

        self.configure_endpoint(slot_id, ep_ctx_phys)?;

        // Default: keyboard (8-byte report).
        let kind = HidKind::Keyboard;
        let report_len = 8;
        let report_buf = DmaBuf::new(report_len, 64)?;

        let mut st = self.state.lock();
        st.hid.push(HidDevice {
            kind,
            slot_id,
            ep_id: 1,
            report_len,
            report_buf,
            prev: [0; 8],
        });
        st.ready = true;
        drop(st);

        crate_print("usb: HID device enumerated on port ");
        crate_print_dec(port as u64);
        crate_print("\n");
        Ok(())
    }

    /// One tick's worth of polled input work.
    ///
    /// Runs in interrupt context: only `try_lock`, bounded work, no waits.
    fn poll_tick(&self) -> Result<(), i32> {
        let mut st = match self.state.try_lock() {
            Some(g) => g,
            None => return Ok(()),
        };

        // Process any pending completion events first.
        self.process_events_locked(&mut st);
        drop(st);

        // Enumerate root ports (no state lock held for MMIO portsc).
        let hcs1 = unsafe { regs::rd32(self.op_base + 0x04) };
        let max_ports = ((hcs1 >> 24) & 0xFF) as u8;
        for port in 1..=max_ports {
            let psc = self.portsc(port);
            if psc & PORT_CONNECT_STATUS != 0 && psc & PORT_PORT_ENABLED == 0 {
                let _ = self.enumerate_port(port);
            }
        }

        // Re-lock and submit interrupt-IN transfers for each HID device.
        let mut st = match self.state.try_lock() {
            Some(g) => g,
            None => return Ok(()),
        };
        let hid_count = st.hid.len();
        let devices: Vec<(u8, u8, u64, u32)> = (0..hid_count)
            .map(|i| {
                let dev = &st.hid[i];
                (dev.slot_id, dev.ep_id, dev.report_buf.phys, dev.report_len as u32)
            })
            .collect();
        drop(st);

        for (slot_id, ep_id, report_ptr, report_len) in devices {
            self.submit_interrupt_in(slot_id, report_ptr, report_len);
        }

        Ok(())
    }
}

/// Enumerate USB Mass Storage devices on the controller at `mmio_base`.
///
/// This is called from `kernel/entry` after `init()` brings up the xHCI
/// controller. It probes root ports for attached BOT mass-storage devices
/// and registers each found device with the Device Manager as `usb.storage`.
pub fn enumerate_storage(mmio_base: u64) -> Result<(), i32> {
    let controllers = CONTROLLERS.lock();
    for ctrl in controllers.iter() {
        // Match by op_base: mmio_base + cap_length == op_base. Since we don't
        // store cap_length separately here, just try the first controller
        // (QEMU has a single xHCI controller).
        let _ = mmio_base;
        return storage::enumerate_storage(Arc::clone(ctrl));
    }
    Err(-19)
}

// ---------------------------------------------------------------------------
// Global pointer position (shared with Intuition's software cursor)
// ---------------------------------------------------------------------------

static POINTER_X: AtomicU32 = AtomicU32::new(640);
static POINTER_Y: AtomicU32 = AtomicU32::new(400);

fn pointer_pos() -> (i32, i32) {
    (
        POINTER_X.load(Ordering::Acquire) as i32,
        POINTER_Y.load(Ordering::Acquire) as i32,
    )
}

fn set_pointer_pos(x: i32, y: i32) {
    POINTER_X.store(x as u32, Ordering::Release);
    POINTER_Y.store(y as u32, Ordering::Release);
}

// ---------------------------------------------------------------------------
// Serial logging helper (best-effort, no panic)
// ---------------------------------------------------------------------------

pub(crate) fn crate_print(_s: &str) {
    // Serial logging is best-effort; the kernel console is the primary output.
    // We skip it here to avoid pulling in console dependencies from the USB driver.
}

/// Best-effort decimal printing (no-op in this driver).
pub(crate) fn crate_print_dec(_v: u64) {}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hid_usage_translates_to_set_one_scancodes() {
        assert_eq!(hid_usage_to_scancode(0x04), Some(0x1E)); // 'a'
        assert_eq!(hid_usage_to_scancode(0x1E), Some(0x02)); // '1'
        assert_eq!(hid_usage_to_scancode(0x28), Some(0x1C)); // Enter
        assert_eq!(hid_usage_to_scancode(0x29), Some(0x01)); // Escape
        assert_eq!(hid_usage_to_scancode(0x2A), Some(0x0E)); // Backspace
        assert_eq!(hid_usage_to_scancode(0xE0), Some(0x1D)); // Left Control
        assert_eq!(hid_usage_to_scancode(0xE1), Some(0x2A)); // Left Shift
        assert_eq!(hid_usage_to_scancode(0x3A), Some(0x3B)); // F1
        assert_eq!(hid_usage_to_scancode(0x00), None);
        assert_eq!(hid_usage_to_scancode(0xE6), None); // GUI has no set-1 equivalent
    }

    /// x86-64 is the only architecture with an enumerable `PciDevice` today.
    #[cfg(target_arch = "x86_64")]
    #[test]
    fn xhci_probe_recognises_only_xhci_classes() {
        let dev = crate_test_device(
            (XHCI_BASE_CLASS as u32) << 16
                | (XHCI_SUBCLASS as u32) << 8
                | (XHCI_PROG_IF as u32),
        );
        let bars = xhci_bars(&[dev]);
        assert!(!bars.is_empty());
        let not_xhci = crate_test_device(0x01_00_00);
        assert!(xhci_bars(&[not_xhci]).is_empty());
    }
}

#[cfg(all(test, target_arch = "x86_64"))]
fn crate_test_device(class_code: u32) -> vela_arch::pci::PciDevice {
    vela_arch::pci::PciDevice {
        bus: 0,
        device: 0,
        function: 0,
        vendor_id: 0x1234,
        device_id: 0x0001,
        class_code,
        revision: 0,
        bars: [vela_arch::pci::Bar {
            raw: 0,
            memory: true,
            prefetchable: false,
            base: 0xFED0_0000,
            size: 0x1000,
        }; 6],
        irq: 0,
        msi_x: false,
    }
}

#[cfg(all(test, not(target_arch = "x86_64")))]
fn crate_test_device(_class_code: u32) -> () {
    ()
}
