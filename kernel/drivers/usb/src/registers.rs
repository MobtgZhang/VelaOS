//! Volatile xHCI MMIO register access.

use core::ptr;

pub(crate) const CAP_LENGTH: usize = 0x00;
pub(crate) const CAP_HCSPARAMS1: usize = 0x04;
pub(crate) const CAP_HCSPARAMS2: usize = 0x08;
pub(crate) const CAP_HCCPARAMS1: usize = 0x10;
pub(crate) const CAP_DBOFF: usize = 0x14;
pub(crate) const CAP_RTSOFF: usize = 0x18;

pub(crate) const OP_USBCMD: usize = 0x00;
pub(crate) const OP_USBSTS: usize = 0x04;
pub(crate) const OP_CRCR: usize = 0x18;
pub(crate) const OP_DCBAA: usize = 0x30;
pub(crate) const OP_CONFIG: usize = 0x38;
pub(crate) const OP_PORTS: usize = 0x400;

pub(crate) const CMD_RUN: u32 = 1 << 0;
pub(crate) const CMD_RESET: u32 = 1 << 1;
pub(crate) const STS_HALTED: u32 = 1 << 0;
pub(crate) const STS_NOT_READY: u32 = 1 << 11;

pub(crate) const PORT_CONNECTION: u32 = 1 << 0;
pub(crate) const PORT_ENABLED: u32 = 1 << 1;
pub(crate) const PORT_RESET: u32 = 1 << 4;
pub(crate) const PORT_POWER: u32 = 1 << 8;
pub(crate) const PORT_CHANGE_BITS: u32 = (1 << 17)
    | (1 << 18)
    | (1 << 19)
    | (1 << 20)
    | (1 << 21)
    | (1 << 22)
    | (1 << 23);

pub(crate) const IR_IMAN: usize = 0x00;
pub(crate) const IR_ERSTSZ: usize = 0x08;
pub(crate) const IR_ERSTBA: usize = 0x10;
pub(crate) const IR_ERDP: usize = 0x18;
pub(crate) const IMAN_IP: u32 = 1 << 0;
pub(crate) const ERDP_EHB: u64 = 1 << 3;

/// Raw MMIO base. Every access is volatile and no reference into MMIO escapes.
#[derive(Clone, Copy)]
pub(crate) struct Mmio {
    base: usize,
}

impl Mmio {
    pub(crate) const fn new(base: u64) -> Self {
        Self { base: base as usize }
    }

    pub(crate) fn read32(self, offset: usize) -> u32 {
        // SAFETY: `init` requires an identity-mapped xHCI BAR; offsets are
        // architectural register offsets within that mapping.
        unsafe { ptr::read_volatile((self.base + offset) as *const u32) }
    }

    pub(crate) fn write32(self, offset: usize, value: u32) {
        // SAFETY: see `read32`; callers select writable registers only.
        unsafe { ptr::write_volatile((self.base + offset) as *mut u32, value) };
    }

    pub(crate) fn read64(self, offset: usize) -> u64 {
        // SAFETY: xHCI's 64-bit registers are naturally aligned.
        unsafe { ptr::read_volatile((self.base + offset) as *const u64) }
    }

    pub(crate) fn write64(self, offset: usize, value: u64) {
        // SAFETY: xHCI's 64-bit registers are naturally aligned.
        unsafe { ptr::write_volatile((self.base + offset) as *mut u64, value) };
    }
}
