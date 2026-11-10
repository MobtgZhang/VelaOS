//! The LoongArch64 interrupt controller.
//!
//! # Two levels of interrupt
//!
//! LoongArch has a *local* interrupt controller built into the core and an
//! *external* one on the system chip. The local one is a handful of bits in
//! `CSR.ECFG` (which lines may interrupt) and `CSR.ESTAT` (which ones did);
//! the external one is the ExtIOI block of the 3A5000 chip, reached over
//! MMIO, which routes the chip's many interrupt lines to the cores.
//!
//! The kernel uses both: the local controller decides whether a line may
//! interrupt *this* hart, and the ExtIOI decides which hart a line goes to.
//!
//! # The line numbers
//!
//! The local lines are the ones the architecture names: 0–1 are the software
//! interrupts, 2–7 the hardware ones, 8–9 the timer and the serial, 11 the
//! I/O interrupt controller, and 12–14 the three "CPU" lines.

/// The local interrupt lines, as `CSR.ECFG.LIE` numbers.
pub mod line {
    /// The first software interrupt line.
    pub const SWI0: u16 = 0;
    /// The second software interrupt line.
    pub const SWI1: u16 = 1;
    /// The first hardware interrupt line.
    pub const HWI0: u16 = 2;
    /// The timer line.
    pub const TIMER: u16 = 11;
    /// The I/O interrupt controller line.
    pub const IO: u16 = 12;
}

/// The ExtIOI register offsets, from the 3A5000 manual's EIOINTC chapter.
///
/// The controller is reached over MMIO; these are the offsets within it.
pub mod eiointc {
    /// The per-core interrupt status registers.
    pub const ISR0: u64 = 0x000;
    /// The per-core interrupt mask registers.
    pub const IMR0: u64 = 0x080;
    /// The node (core) count.
    pub const NODES: u64 = 0x100;
    /// The controller's global enable.
    pub const EIOINTC_EN: u64 = 0x200;
    /// The per-core enable.
    pub const CORE_EN: u64 = 0x300;
    /// The isolation mask.
    pub const ISOLATION: u64 = 0x400;
    /// The per-core local interrupt enable.
    pub const LOCAL_EN: u64 = 0x480;
    /// The global interrupt enable.
    pub const GLOBAL_EN: u64 = 0x580;
    /// The core map: which core a line belongs to.
    pub const CORE_MAP: u64 = 0x600;
    /// The first of the eight SPI (chip-level) interrupt inputs.
    pub const SPI_IP0: u64 = 0x700;
    /// The stride between the SPI input registers.
    pub const SPI_IP_STRIDE: u64 = 0x80;
    /// The number of SPI inputs.
    pub const SPI_INPUTS: u64 = 8;
}

/// The offset of a core's interrupt status register.
pub const fn isr_offset(core: u64) -> u64 {
    eiointc::ISR0 + core * 8
}

/// The offset of a core's interrupt mask register.
pub const fn imr_offset(core: u64) -> u64 {
    eiointc::IMR0 + core * 8
}

/// The offset of the SPI input register for `line`.
pub const fn spi_ip_offset(line: u64) -> u64 {
    eiointc::SPI_IP0 + line * eiointc::SPI_IP_STRIDE
}

/// Read one 32-bit ExtIOI register.
unsafe fn read32(base: u64, offset: u64) -> u32 {
    if !crate::regs::ON_BARE_METAL {
        return 0;
    }
    // SAFETY: the caller names a controller register.
    unsafe { core::ptr::read_volatile((base + offset) as *const u32) }
}

/// Write one 32-bit ExtIOI register.
unsafe fn write32(base: u64, offset: u64, value: u32) {
    if !crate::regs::ON_BARE_METAL {
        return;
    }
    // SAFETY: the caller names a controller register.
    unsafe { core::ptr::write_volatile((base + offset) as *mut u32, value) }
}

/// The number of interrupt lines the local controller has.
pub const LOCAL_LINES: u16 = 15;

/// The `CSR.ECFG.LIE` mask that enables exactly one line.
///
/// The local interrupt enable is a bit per line, so enabling a line is a
/// read-modify-write of one bit.
pub const fn lie_for(line: u16) -> u64 {
    1u64 << line
}

/// True when `line` may interrupt this hart.
pub fn line_enabled(line: u16) -> bool {
    if !crate::regs::ON_BARE_METAL {
        return false;
    }
    let ecfg = crate::regs::ecfg();
    ecfg & lie_for(line) != 0
}

/// Allow `line` to interrupt this hart.
///
/// # Safety
///
/// The caller must own the interrupt-mask hazard: enabling a line that
/// fires immediately can call a handler while a lock is held.
pub unsafe fn enable_line(line: u16) {
    if !crate::regs::ON_BARE_METAL {
        return;
    }
    // SAFETY: the caller owns the hazard.
    unsafe { crate::regs::ecfg_set_lie((crate::regs::ecfg() as u16) | line) }
}

/// The local interrupt lines that are pending.
pub fn pending_lines() -> u16 {
    if !crate::regs::ON_BARE_METAL {
        return 0;
    }
    crate::regs::estat() as u16 & 0x7FFF
}

/// Enable the controller as a whole and route the chip's lines.
///
/// `base` is the physical address of the ExtIOI block, which is a property
/// of the machine.
///
/// # Safety
///
/// The caller must have the controller's register map right: enabling a
/// controller whose address is wrong programs whatever is there.
pub unsafe fn init(base: u64) {
    if !crate::regs::ON_BARE_METAL {
        return;
    }
    // SAFETY: the caller guarantees the base address.
    unsafe {
        // The controller itself, then the cores, then the global enable.
        write32(base, eiointc::EIOINTC_EN, 0x1);
        write32(base, eiointc::CORE_EN, 0x1);
        write32(base, eiointc::GLOBAL_EN, 0x1);
        write32(base, eiointc::LOCAL_EN, 0x1);
    }
}

/// The first pending external interrupt on `core`, if any.
///
/// The controller reports the pending lines in its status register; the
/// lowest set bit is the one the kernel services first, which is the order
/// the 3A5000 documents for its priority scheme.
pub fn pending_external(base: u64, core: u64) -> Option<u32> {
    if !crate::regs::ON_BARE_METAL {
        return None;
    }
    // SAFETY: the caller guarantees the base address.
    let status = unsafe { read32(base, isr_offset(core)) };
    if status == 0 {
        return None;
    }
    Some(status.trailing_zeros())
}

/// Acknowledge an external interrupt by clearing its status bit.
///
/// # Safety
///
/// The caller must have serviced the interrupt.
pub unsafe fn acknowledge(base: u64, core: u64, line: u32) {
    if !crate::regs::ON_BARE_METAL {
        return;
    }
    // SAFETY: the caller guarantees the base address.
    unsafe { write32(base, isr_offset(core), 1 << line) }
}

/// Send an inter-processor interrupt to `core`.
///
/// The chip's software interrupt lines are the two "SWI" lines, and the
/// kernel uses the first of them to wake another hart. The ExtIOI routes a
/// line to a core through that core's *local enable* register: setting the
/// line's bit there makes the controller present it to the core, and the
/// core's own `CSR.ECFG.LIE` then decides whether it interrupts.
///
/// `base` is the physical address of the ExtIOI block.
///
/// # Safety
///
/// The caller must have the controller's register map right.
pub unsafe fn send_ipi(base: u64, core: u64) {
    if !crate::regs::ON_BARE_METAL {
        return;
    }
    // SAFETY: the caller guarantees the base address.
    unsafe {
        // The line's bit in the core's local enable, and the same bit in
        // the core's mask (the mask is written with ones meaning "not
        // masked", so a zero leaves the line unmasked).
        let enable = read32(base, eiointc::LOCAL_EN);
        write32(base, eiointc::LOCAL_EN, enable | lie_for(line::SWI0) as u32);
        let mask = read32(base, imr_offset(core));
        write32(base, imr_offset(core), mask & !(1 << line::SWI0));
    }
}

#[cfg(test)]
extern crate std;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_local_line_numbers_are_the_architectures() {
        assert_eq!(line::SWI0, 0);
        assert_eq!(line::SWI1, 1);
        assert_eq!(line::HWI0, 2);
        assert_eq!(line::TIMER, 11);
        assert_eq!(line::IO, 12);
        assert_eq!(LOCAL_LINES, 15);
    }

    #[test]
    fn the_enable_mask_is_one_bit_per_line() {
        assert_eq!(lie_for(line::SWI0), 1);
        assert_eq!(lie_for(line::TIMER), 1 << 11);
        assert_eq!(lie_for(line::IO), 1 << 12);
    }

    #[test]
    fn the_controller_offsets_are_the_3a5000_ones() {
        assert_eq!(eiointc::ISR0, 0x000);
        assert_eq!(eiointc::IMR0, 0x080);
        assert_eq!(eiointc::NODES, 0x100);
        assert_eq!(eiointc::EIOINTC_EN, 0x200);
        assert_eq!(eiointc::CORE_EN, 0x300);
        assert_eq!(eiointc::ISOLATION, 0x400);
        assert_eq!(eiointc::LOCAL_EN, 0x480);
        assert_eq!(eiointc::GLOBAL_EN, 0x580);
        assert_eq!(eiointc::CORE_MAP, 0x600);
        assert_eq!(eiointc::SPI_IP0, 0x700);
    }

    #[test]
    fn the_per_core_registers_stride_by_eight() {
        assert_eq!(isr_offset(0), 0x000);
        assert_eq!(isr_offset(1), 0x008);
        assert_eq!(imr_offset(2), 0x090);
    }

    #[test]
    fn the_spi_inputs_stride_by_a_word_block() {
        assert_eq!(spi_ip_offset(0), 0x700);
        assert_eq!(spi_ip_offset(1), 0x780);
        assert_eq!(spi_ip_offset(7), 0xA80);
    }

    #[test]
    fn the_controller_is_inert_off_bare_metal() {
        assert!(!line_enabled(line::TIMER));
        assert_eq!(pending_lines(), 0);
        assert_eq!(pending_external(0x1000, 0), None);
    }
}
