//! The AArch64 PCI configuration space.
//!
//! # The extended configuration space
//!
//! PCI configuration space is reached through two mechanisms: the legacy
//! ports and the *extended* mechanism, which maps the whole configuration
//! space into memory. The extended mechanism is the one worth using: it
//! needs no port I/O, it is faster, and it is how the kernel reaches a
//! device's extended registers.
//!
//! The address of the window is a property of the machine. QEMU's `virt`
//! machine places it at `0x4010_0000`, with the bus number encoded in the
//! top byte of the address — the same shape every architecture's ECAM uses.

/// The default base of the extended configuration space.
///
/// QEMU's `virt` machine maps it here.
pub const ECAM_BASE: u64 = 0x3F00_0000;

/// The physical address of a device's configuration register.
///
/// The address is
/// `base + (bus << 20) + (device << 15) + (function << 12) + offset`,
/// which is the ECAM layout every architecture uses. The fields are added
/// rather than or-ed, because they live inside the window: a base whose own
/// bits overlap the bus field would swallow the bus number.
pub const fn ecam_address(base: u64, bus: u8, device: u8, function: u8, offset: u16) -> u64 {
    base + ((bus as u64) << 20)
        + ((device as u64) << 15)
        + ((function as u64) << 12)
        + (offset as u64 & 0xFFF)
}

/// Read a 32-bit configuration register.
pub fn read_dword(base: u64, bus: u8, device: u8, function: u8, offset: u16) -> u32 {
    if !crate::regs::ON_BARE_METAL {
        // No machine: the all-ones "no device" value is the honest answer
        // for every probe.
        return 0xFFFF_FFFF;
    }
    let address = ecam_address(base, bus, device, function, offset);
    // SAFETY: the address is inside the configuration window, which the
    // caller has told us about.
    unsafe { core::ptr::read_volatile(address as *const u32) }
}

/// Read a 16-bit configuration register.
pub fn read_word(base: u64, bus: u8, device: u8, function: u8, offset: u16) -> u16 {
    if !crate::regs::ON_BARE_METAL {
        return 0xFFFF;
    }
    let address = ecam_address(base, bus, device, function, offset);
    // SAFETY: as above.
    unsafe { core::ptr::read_volatile(address as *const u16) }
}

/// Read an 8-bit configuration register.
pub fn read_byte(base: u64, bus: u8, device: u8, function: u8, offset: u16) -> u8 {
    if !crate::regs::ON_BARE_METAL {
        return 0xFF;
    }
    let address = ecam_address(base, bus, device, function, offset);
    // SAFETY: as above.
    unsafe { core::ptr::read_volatile(address as *const u8) }
}

/// The vendor and device identifiers of a function.
pub fn vendor_device(base: u64, bus: u8, device: u8, function: u8) -> (u16, u16) {
    let vendor = read_word(base, bus, device, function, 0);
    let device_id = read_word(base, bus, device, function, 2);
    (vendor, device_id)
}

/// True when the function is present.
///
/// A function that is not present reads back as all ones, which is the
/// classic "no device here" answer.
pub fn is_present(base: u64, bus: u8, device: u8, function: u8) -> bool {
    read_word(base, bus, device, function, 0) != 0xFFFF
}

#[cfg(test)]
extern crate std;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_address_layout_is_the_ecam_one() {
        let address = ecam_address(ECAM_BASE, 0, 0, 0, 0);
        assert_eq!(address, ECAM_BASE);
        // Device 1 sits 32 KiB along; function 1 sits 4 KiB along.
        assert_eq!(ecam_address(ECAM_BASE, 0, 1, 0, 0), ECAM_BASE + (1 << 15));
        assert_eq!(ecam_address(ECAM_BASE, 0, 0, 1, 0), ECAM_BASE + (1 << 12));
        // The bus number is in the top byte.
        assert_eq!(ecam_address(ECAM_BASE, 1, 0, 0, 0), ECAM_BASE + (1 << 20));
    }

    #[test]
    fn the_offset_stays_inside_the_register() {
        let address = ecam_address(ECAM_BASE, 0, 0, 0, 0xFFF);
        assert_eq!(address, ECAM_BASE + 0xFFF);
    }

    #[test]
    fn the_probes_are_inert_off_bare_metal() {
        assert_eq!(read_dword(ECAM_BASE, 0, 0, 0, 0), 0xFFFF_FFFF);
        assert_eq!(read_word(ECAM_BASE, 0, 0, 0, 0), 0xFFFF);
        assert_eq!(read_byte(ECAM_BASE, 0, 0, 0, 0), 0xFF);
        assert!(!is_present(ECAM_BASE, 0, 0, 0));
    }
}
