//! Transfer Request Blocks and producer-side transfer rings.

use crate::dma::DmaBuffer;
use core::ptr;
use core::sync::atomic::{fence, Ordering};

pub(crate) const TRB_TYPE_NORMAL: u32 = 1;
pub(crate) const TRB_TYPE_SETUP: u32 = 2;
pub(crate) const TRB_TYPE_DATA: u32 = 3;
pub(crate) const TRB_TYPE_STATUS: u32 = 4;
pub(crate) const TRB_TYPE_LINK: u32 = 6;
pub(crate) const TRB_TYPE_ENABLE_SLOT: u32 = 9;
pub(crate) const TRB_TYPE_DISABLE_SLOT: u32 = 10;
pub(crate) const TRB_TYPE_ADDRESS_DEVICE: u32 = 11;
pub(crate) const TRB_TYPE_CONFIGURE_ENDPOINT: u32 = 12;
pub(crate) const TRB_TYPE_EVALUATE_CONTEXT: u32 = 13;
pub(crate) const TRB_TYPE_TRANSFER_EVENT: u32 = 32;
pub(crate) const TRB_TYPE_COMMAND_COMPLETION: u32 = 33;
pub(crate) const TRB_TYPE_PORT_STATUS: u32 = 34;

pub(crate) const COMPLETION_SUCCESS: u32 = 1;
pub(crate) const COMPLETION_SHORT_PACKET: u32 = 13;

const TRB_CYCLE: u32 = 1 << 0;
const TRB_TOGGLE_CYCLE: u32 = 1 << 1;
const TRB_ISP: u32 = 1 << 2;
const TRB_IOC: u32 = 1 << 5;
const TRB_IDT: u32 = 1 << 6;
const TRB_DIR_IN: u32 = 1 << 16;

/// The native little-endian, 16-byte layout shared by every xHCI ring entry.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(C)]
pub(crate) struct Trb {
    pub(crate) parameter: u64,
    pub(crate) status: u32,
    pub(crate) control: u32,
}

impl Trb {
    pub(crate) const fn new(parameter: u64, status: u32, control: u32) -> Self {
        Self {
            parameter,
            status,
            control,
        }
    }

    pub(crate) const fn command(kind: u32, parameter: u64, slot_id: u8) -> Self {
        Self::new(parameter, 0, (kind << 10) | ((slot_id as u32) << 24))
    }

    pub(crate) const fn normal(buffer: u64, length: u32) -> Self {
        Self::new(
            buffer,
            length,
            (TRB_TYPE_NORMAL << 10) | TRB_IOC | TRB_ISP,
        )
    }

    /// Pack the eight-byte USB setup packet into a Setup Stage TRB.
    pub(crate) const fn setup(request_type: u8, request: u8, value: u16, index: u16, length: u16) -> Self {
        let parameter = request_type as u64
            | (request as u64) << 8
            | (value as u64) << 16
            | (index as u64) << 32
            | (length as u64) << 48;
        let transfer_type = if length == 0 {
            0
        } else if request_type & 0x80 != 0 {
            3
        } else {
            2
        };
        Self::new(
            parameter,
            8,
            (TRB_TYPE_SETUP << 10) | TRB_IDT | (transfer_type << 16),
        )
    }

    pub(crate) const fn data(buffer: u64, length: u32, direction_in: bool) -> Self {
        Self::new(
            buffer,
            length,
            (TRB_TYPE_DATA << 10) | if direction_in { TRB_DIR_IN } else { 0 },
        )
    }

    /// Build the Status Stage. Its direction is opposite to the data stage.
    pub(crate) const fn status(data_in: bool) -> Self {
        Self::new(
            0,
            0,
            (TRB_TYPE_STATUS << 10)
                | TRB_IOC
                | if data_in { 0 } else { TRB_DIR_IN },
        )
    }

    pub(crate) const fn kind(self) -> u32 {
        (self.control >> 10) & 0x3f
    }

    pub(crate) const fn completion_code(self) -> u32 {
        (self.status >> 24) & 0xff
    }

    pub(crate) const fn residual(self) -> usize {
        (self.status & 0x00ff_ffff) as usize
    }

    pub(crate) const fn slot_id(self) -> u8 {
        (self.control >> 24) as u8
    }

    pub(crate) const fn endpoint_id(self) -> u8 {
        ((self.control >> 16) & 0x1f) as u8
    }

    pub(crate) const fn cycle(self) -> bool {
        self.control & TRB_CYCLE != 0
    }
}

/// A 256-entry single-segment ring. The final entry is its Link TRB.
pub(crate) const RING_ENTRIES: usize = 256;

pub(crate) struct Ring {
    memory: DmaBuffer,
    enqueue: usize,
    cycle: bool,
}

impl Ring {
    pub(crate) fn new() -> Result<Self, i32> {
        Ok(Self {
            memory: DmaBuffer::new(RING_ENTRIES * core::mem::size_of::<Trb>(), 4096)?,
            enqueue: 0,
            // Hardware starts consuming with cycle state one.
            cycle: true,
        })
    }

    pub(crate) fn addr(&self) -> u64 {
        self.memory.addr()
    }

    pub(crate) fn cycle(&self) -> bool {
        self.cycle
    }

    fn trb_ptr(&self, index: usize) -> *mut Trb {
        // SAFETY: callers only supply indices below RING_ENTRIES, and the DMA
        // allocation contains exactly that many properly aligned TRBs.
        unsafe { self.memory.as_mut_ptr().cast::<Trb>().add(index) }
    }

    pub(crate) fn read(&self, index: usize) -> Trb {
        // SAFETY: all ring indices are bounded by the ring implementation.
        unsafe { ptr::read_volatile(self.trb_ptr(index)) }
    }

    fn write(&self, index: usize, mut trb: Trb) {
        trb.control = (trb.control & !TRB_CYCLE) | u32::from(self.cycle);
        // SAFETY: all ring indices are bounded by the ring implementation.
        unsafe { ptr::write_volatile(self.trb_ptr(index), trb) };
    }

    /// Enqueue a TRB and return its DMA address and producer index.
    pub(crate) fn push(&mut self, trb: Trb) -> (u64, usize) {
        let index = self.enqueue;
        let address = self.addr() + (index * core::mem::size_of::<Trb>()) as u64;
        self.write(index, trb);
        self.enqueue += 1;
        if self.enqueue == RING_ENTRIES - 1 {
            // The Link TRB is written with the old cycle and toggles the
            // producer cycle for entries that follow it.
            self.write(
                RING_ENTRIES - 1,
                Trb::new(
                    self.addr(),
                    0,
                    (TRB_TYPE_LINK << 10) | TRB_TOGGLE_CYCLE,
                ),
            );
            self.enqueue = 0;
            self.cycle = !self.cycle;
        }
        fence(Ordering::Release);
        (address, index)
    }

    /// Reuse a completed one-TRB interrupt transfer at its original position.
    pub(crate) fn requeue(&self, index: usize, trb: Trb) {
        if index < RING_ENTRIES - 1 {
            self.write(index, trb);
            fence(Ordering::Release);
        }
    }
}

/// USB standard/class request fields passed to [`Trb::setup`].
#[derive(Clone, Copy)]
pub(crate) struct SetupPacket {
    pub(crate) request_type: u8,
    pub(crate) request: u8,
    pub(crate) value: u16,
    pub(crate) index: u16,
    pub(crate) length: u16,
}

impl SetupPacket {
    pub(crate) const fn trb(self) -> Trb {
        Trb::setup(
            self.request_type,
            self.request,
            self.value,
            self.index,
            self.length,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn setup_trb_contains_the_complete_usb_request() {
        let trb = Trb::setup(0x80, 6, 0x0100, 0, 18);
        assert_eq!(trb.kind(), TRB_TYPE_SETUP);
        assert_eq!(trb.status, 8);
        assert_eq!(trb.parameter, 0x0012_0000_0100_0680);
        assert_ne!(trb.control & TRB_IDT, 0);
        assert_eq!((trb.control >> 16) & 3, 3);
    }

    #[test]
    fn normal_trb_requests_completion_and_short_packet_events() {
        let trb = Trb::normal(0x1234_5000, 8);
        assert_eq!(trb.kind(), TRB_TYPE_NORMAL);
        assert_eq!(trb.parameter, 0x1234_5000);
        assert_eq!(trb.status, 8);
        assert_ne!(trb.control & TRB_IOC, 0);
        assert_ne!(trb.control & TRB_ISP, 0);
    }

    #[test]
    fn status_direction_is_opposite_to_data_direction() {
        assert_eq!(Trb::status(true).control & TRB_DIR_IN, 0);
        assert_ne!(Trb::status(false).control & TRB_DIR_IN, TRB_DIR_IN);
    }
}
