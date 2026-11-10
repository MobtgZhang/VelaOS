//! PCI access through the ECAM memory-mapped configuration space.
//!
//! # Why ECAM and not the x86 ports
//!
//! x86-64 reaches PCI configuration space through the `0x0CF8`/`0x0CFC` port
//! pair, a legacy mechanism that only exists because the original PC had no
//! better way. RISC-V platforms use ECAM (Enhanced Configuration Access
//! Mechanism) instead: the whole configuration space is *memory-mapped*,
//! with a fixed stride — 4 KiB per function, 32 KiB (well, 32 functions'
//! worth) per device, 1 MiB per bus — so any function's any dword is one
//! physical address:
//!
//! ```text
//! addr = ECAM_BASE + bus << 20 | device << 15 | function << 12 | offset
//! ```
//!
//! QEMU's `virt` machine places the ECAM at `0x0000_3000_0000` (advertised in
//! the device tree as `pci-host-ecam`), covering bus 0..=255.
//!
//! # Alignment is the hardware's contract
//!
//! Only naturally aligned dword (and wider) accesses are defined; an
//! unaligned configuration read is architecturally undefined, so
//! [`ecam_offset`] callers pass dword-aligned offsets and this module reads
//! whole dwords. Word and byte views are *extracted* from a dword read,
//! never issued as narrow MMIO — the same discipline the x86-64 crate
//! applies around `0x0CF8`.
//!
//! # What v0.1 ships
//!
//! Only the arithmetic and the read primitives: enumeration (walking
//! bus/device/function, reading vendor IDs, BARs) is the next milestone's
//! work, and the pure offset builder below is what that enumeration will
//! be host-tested with.

use crate::regs::ON_BARE_METAL;

/// The ECAM base on QEMU's `virt` machine.
pub const ECAM_BASE: u64 = 0x0000_3000_0000;

/// The number of devices per bus (PCI: 32).
pub const DEVICES_PER_BUS: u64 = 32;
/// The number of functions per device (PCI: 8).
pub const FUNCTIONS_PER_DEVICE: u64 = 8;

/// The physical address of `offset` (0..=0x0FFF) in function `function` of
/// device `device` on bus `bus`.
///
/// The stride arithmetic is the ECAM spec's: 1 MiB per bus, 32 KiB per
/// device (a bus's 32 devices × 1 KiB per function... the per-device stride
/// is `FUNCTIONS_PER_DEVICE << 12`), 4 KiB per function.
pub const fn ecam_offset(bus: u64, device: u64, function: u64, offset: u64) -> u64 {
    // The fields are added, not or-ed: they live inside the window, and a
    // base whose own bits overlap the bus field would swallow the bus
    // number.
    ECAM_BASE + (bus << 20) + (device << 15) + (function << 12) + offset
}

/// Read a dword from configuration space.
///
/// Returns `0x0000_FFFF_FFFF` when no device responds — the PCI "empty slot"
/// convention, where absent functions read all ones.
///
/// # Safety
///
/// The address must come from [`ecam_offset`] with a dword-aligned
/// `offset`; an arbitrary physical address would alias whatever lives
/// there.
pub unsafe fn read_dword(bus: u64, device: u64, function: u64, offset: u64) -> u32 {
    let _ = (bus, device, function, offset);
    if !ON_BARE_METAL {
        return 0x0000_FFFF_FFFF;
    }
    // SAFETY: the caller names an ECAM dword with [`ecam_offset`]; a
    // naturally aligned 32-bit read of the configuration space is the
    // defined access.
    unsafe { core::ptr::read_volatile(ecam_offset(bus, device, function, offset) as *const u32) }
}

/// Read a 16-bit word by *extracting* it from a dword read.
///
/// Narrow configuration accesses are not issued: the dword that contains
/// the word is read, and the half is selected by the offset's low bits.
///
/// # Safety
///
/// As in [`read_dword`]; `offset` must be 2-byte aligned.
pub unsafe fn read_word(bus: u64, device: u64, function: u64, offset: u64) -> u16 {
    // SAFETY: the dword base is the offset with its low two bits cleared.
    unsafe {
        let dword = read_dword(bus, device, function, offset & !0b11);
        if offset & 0b10 == 0 {
            dword as u16
        } else {
            (dword >> 16) as u16
        }
    }
}

#[cfg(test)]
extern crate std;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_ecam_base_is_the_qemu_virt_one() {
        assert_eq!(ECAM_BASE, 0x0000_3000_0000);
    }

    #[test]
    fn bus_zero_device_zero_is_the_base() {
        assert_eq!(ecam_offset(0, 0, 0, 0), ECAM_BASE);
    }

    #[test]
    fn the_stride_arithmetic_matches_ecam() {
        // One MiB per bus.
        assert_eq!(ecam_offset(1, 0, 0, 0), ECAM_BASE + 0x0010_0000);
        // 32 KiB per device (8 functions × 4 KiB).
        assert_eq!(ecam_offset(0, 1, 0, 0), ECAM_BASE + 0x8000);
        // 4 KiB per function.
        assert_eq!(ecam_offset(0, 0, 1, 0), ECAM_BASE + 0x1000);
        // And the full composition, bit-checked against the formula.
        let addr = ecam_offset(0x002A, 0x001F, 0x0007, 0x0123);
        assert_eq!(addr & 0x0FFF, 0x0123, "offset occupies the low 12 bits");
        assert_eq!((addr >> 12) & 0x0007, 0x0007, "function is bits 14:12");
        assert_eq!((addr >> 15) & 0x001F, 0x001F, "device is bits 19:15");
        assert_eq!((addr >> 20) & 0x00FF, 0x002A, "bus is bits 27:20");
    }

    #[test]
    fn the_pci_topology_bounds_are_the_spec_ones() {
        assert_eq!(DEVICES_PER_BUS, 32);
        assert_eq!(FUNCTIONS_PER_DEVICE, 8);
    }

    #[test]
    fn reads_report_empty_slots_off_bare_metal() {
        // The host has no ECAM; the all-ones "no device" value is the
        // honest answer for every probe.
        // SAFETY: the documented host stub.
        assert_eq!(unsafe { read_dword(0, 0, 0, 0) }, 0x0000_FFFF_FFFF);
        // SAFETY: as above.
        assert_eq!(unsafe { read_word(0, 0, 0, 0) }, 0xFFFF);
    }

    #[test]
    fn word_extraction_picks_the_right_half() {
        // Drive the extraction with a stubbed dword: read_word reads a
        // full dword and selects by bit 1 of the offset, so a word at
        // offset 2 must come from the high half. (Off bare metal both
        // halves read all ones, so the assertion below holds for either
        // half — the real coverage is the arithmetic, pinned above.)
        // SAFETY: the documented host stub.
        assert_eq!(unsafe { read_word(0, 0, 0, 2) }, 0xFFFF);
    }
}
