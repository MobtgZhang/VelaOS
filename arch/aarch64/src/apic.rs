//! The AArch64 interrupt controller: GICv3.
//!
//! # The two halves
//!
//! GICv3 is a distributed interrupt controller. The **distributor** holds
//! the configuration for every interrupt source in the system — its
//! priority, its target CPU and its enable — and the **redistributor** and
//! **CPU interface** handle the per-CPU side: the SGI and PPI sources, and
//! the acknowledgement of the interrupts a CPU takes.
//!
//! # The register spaces
//!
//! The distributor and the redistributors live in one frame, reached
//! through the redistributor's frame base; the CPU interface is reached
//! through the system registers `ICC_IGRPEN1_EL1`, `ICC_IAR1_EL1` and
//! `ICC_EOIR1_EL1`.
//!
//! # The interrupt numbers
//!
//! The first thirty-two interrupts are the *private* ones: 0–15 are the
//! software-generated interrupts (SGIs) and 16–31 the pre-programmed
//! interrupts (PPIs) — the timer, the serial, the interrupt controller
//! itself. Everything from 32 up is a shared peripheral interrupt (SPI),
//! one per device.

/// The distributor's registers, relative to the redistributor frame base.
pub mod distributor {
    /// The distributor's type register.
    pub const TYPER: u64 = 0x008;
    /// The control register: enable the distributor.
    pub const CTLR: u64 = 0x000;
    /// The interrupt group register.
    pub const IGROUPR: u64 = 0x080;
    /// The interrupt set-enable register.
    pub const ISENABLER: u64 = 0x100;
    /// The interrupt set-pending register.
    pub const ISPENDR: u64 = 0x200;
    /// The interrupt priority register.
    pub const IPRIORITYR: u64 = 0x400;
    /// The interrupt routing register: which CPU takes an SPI.
    pub const ITARGETSR: u64 = 0x800;
    /// The software-generated interrupt register.
    pub const SGIR: u64 = 0xF00;
}

/// The redistributor's registers, relative to the redistributor frame base.
pub mod redistributor {
    /// The redistributor's type register.
    pub const TYPER: u64 = 0x008;
    /// The redistributor's wake-up register.
    pub const WAKER: u64 = 0x014;
    /// The base of the SGI (software-generated interrupt) registers.
    pub const SGI: u64 = 0x100;
}

/// The offset of the redistributor for `cpu` within the frame.
pub const fn redistributor_offset(cpu: u64) -> u64 {
    0x20000 + cpu * 0x20000
}

/// The SGI set-enable register of the redistributor.
pub const fn sgi_set_enable(cpu: u64) -> u64 {
    redistributor::SGI + redistributor_offset(cpu)
}

/// The SGI pending register of the redistributor.
pub const fn sgi_pending(cpu: u64) -> u64 {
    redistributor::SGI + redistributor_offset(cpu) + 4
}

/// The set-enable register for a shared peripheral interrupt.
///
/// The registers are one per thirty-two interrupts, and the *first* one
/// covers the shared interrupts 32..63 — the private ones are not in the
/// distributor's shared registers at all. So interrupt `n` lives in register
/// `(n - 32) / 32` at bit `n % 32`.
pub const fn spi_set_enable(interrupt: u32) -> u64 {
    distributor::ISENABLER + ((interrupt.saturating_sub(32) / 32) as u64) * 4
}

/// The priority register for a shared peripheral interrupt.
pub const fn spi_priority(interrupt: u32) -> u64 {
    distributor::IPRIORITYR + ((interrupt.saturating_sub(32) / 32) as u64) * 4
}

/// The routing register for a shared peripheral interrupt.
pub const fn spi_target(interrupt: u32) -> u64 {
    distributor::ITARGETSR + ((interrupt.saturating_sub(32) / 32) as u64) * 4
}

/// The number of interrupts the private range holds.
pub const PRIVATE_INTERRUPTS: u32 = 32;

/// True when the interrupt is one of the private ones.
pub const fn is_private(interrupt: u32) -> bool {
    interrupt < PRIVATE_INTERRUPTS
}

/// Read one 32-bit GIC register.
unsafe fn read32(base: u64, offset: u64) -> u32 {
    if !crate::regs::ON_BARE_METAL {
        return 0;
    }
    // SAFETY: the caller names a GIC register.
    unsafe { core::ptr::read_volatile((base + offset) as *const u32) }
}

/// Write one 32-bit GIC register.
unsafe fn write32(base: u64, offset: u64, value: u32) {
    if !crate::regs::ON_BARE_METAL {
        return;
    }
    // SAFETY: the caller names a GIC register.
    unsafe { core::ptr::write_volatile((base + offset) as *mut u32, value) }
}

/// Set up the interrupt controller.
///
/// `base` is the physical address of the redistributor frame — the frame
/// that holds the distributor and every redistributor.
///
/// # Safety
///
/// The caller must have the controller's register map right.
pub unsafe fn init(base: u64) {
    if !crate::regs::ON_BARE_METAL {
        return;
    }
    // SAFETY: the caller guarantees the base address.
    unsafe {
        // Remember the frame, so the interrupt helpers can find it.
        GIC_BASE.store(base, core::sync::atomic::Ordering::Release);
        // The distributor first, then this CPU's redistributor.
        write32(base, distributor::CTLR, 1);
        write32(base, redistributor::WAKER, 0);
        // Let the system registers reach the GIC's CPU interface.
        crate::regs::write_sysreg("icc_sre_el1", 3);
        crate::regs::isb();
    }
}

/// The number of interrupts the controller implements.
pub fn interrupt_count(base: u64) -> u32 {
    if !crate::regs::ON_BARE_METAL {
        return 0;
    }
    // SAFETY: the distributor's type register is readable.
    let value = unsafe { read32(base, distributor::TYPER) };
    // Bits 19:16 hold the number of interrupt lines minus 32.
    32 + ((value >> 16) & 0xF) * 32
}

/// Allow an interrupt to interrupt this CPU.
pub fn enable_interrupt(interrupt: u32) {
    if !crate::regs::ON_BARE_METAL {
        return;
    }
    if is_private(interrupt) {
        // The private interrupts are enabled through the redistributor.
        let cpu = crate::current_cpu_id() as u64;
        // SAFETY: the register is named by the architecture.
        unsafe {
            crate::regs::write_sysreg("icc_sgi_eoi", 0);
        }
        let base = GIC_BASE.load(core::sync::atomic::Ordering::Acquire);
        if base != 0 {
            // SAFETY: the base address was set by `init`.
            unsafe { write32(base, sgi_set_enable(cpu), 1 << (interrupt % 32)) };
        }
    } else {
        let base = GIC_BASE.load(core::sync::atomic::Ordering::Acquire);
        if base == 0 {
            return;
        }
        // SAFETY: the base address was set by `init`.
        unsafe { write32(base, spi_set_enable(interrupt), 1 << (interrupt % 32)) };
    }
}

/// The base of the redistributor frame, remembered from `init`.
static GIC_BASE: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// The physical address of the interrupt controller's frame.
pub fn base() -> u64 {
    GIC_BASE.load(core::sync::atomic::Ordering::Acquire)
}

/// Take the next pending interrupt.
///
/// Returns the interrupt number, or `None` when nothing is pending.
///
/// # Safety
///
/// The caller must acknowledge the interrupt it takes, with
/// [`acknowledge`].
pub unsafe fn acknowledge_next() -> Option<u32> {
    if !crate::regs::ON_BARE_METAL {
        return None;
    }
    // SAFETY: `ICC_IAR1_EL1` reads the next interrupt; the value is only
    // meaningful when the controller is enabled.
    let interrupt = unsafe { crate::regs::read_sysreg("icc_iar1_el1") } as u32;
    // 1020 is the "no interrupt" value the architecture defines.
    if interrupt >= 1020 {
        return None;
    }
    Some(interrupt)
}

/// Finish handling an interrupt.
///
/// # Safety
///
/// The interrupt must be one that was taken with [`acknowledge_next`].
pub unsafe fn acknowledge(interrupt: u32) {
    if !crate::regs::ON_BARE_METAL {
        return;
    }
    // SAFETY: the end-of-interrupt register takes the interrupt number.
    unsafe { crate::regs::write_sysreg("icc_eoir1_el1", interrupt as u64) };
}

/// Send an inter-processor interrupt to `cpu`.
///
/// The software-generated interrupt 0 is the kernel's "wake this core" door.
pub fn send_ipi(cpu: u32) {
    if !crate::regs::ON_BARE_METAL {
        return;
    }
    let base = base();
    if base == 0 {
        return;
    }
    // SAFETY: the register is named by the architecture.
    unsafe {
        // The target list in the upper bits, the interrupt id in the lower
        // eight, and the "send" bit in bit 31.
        let target = (cpu as u64) << 16;
        crate::regs::write_sysreg("icc_sgi_int", target);
    }
}

#[cfg(test)]
extern crate std;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_private_range_is_the_first_thirty_two() {
        assert!(is_private(0));
        assert!(is_private(31));
        assert!(!is_private(32));
    }

    #[test]
    fn the_shared_registers_stride_by_thirty_two_interrupts() {
        // The first shared register covers interrupts 32..63, and the next
        // one covers 64..95.
        assert_eq!(spi_set_enable(32), distributor::ISENABLER);
        assert_eq!(spi_set_enable(63), distributor::ISENABLER);
        assert_eq!(spi_set_enable(64), distributor::ISENABLER + 4);
        assert_eq!(spi_set_enable(95), distributor::ISENABLER + 4);
        assert_eq!(spi_set_enable(96), distributor::ISENABLER + 8);
        // The priority and routing registers follow the same grouping.
        assert_eq!(spi_priority(32), distributor::IPRIORITYR);
        assert_eq!(spi_target(32), distributor::ITARGETSR);
    }

    #[test]
    fn the_redistributor_frames_stride_by_128_kilobytes() {
        assert_eq!(redistributor_offset(0), 0x20000);
        assert_eq!(redistributor_offset(1), 0x40000);
        assert_eq!(sgi_set_enable(0), redistributor::SGI + 0x20000);
    }

    #[test]
    fn the_controller_is_inert_off_bare_metal() {
        assert_eq!(interrupt_count(0), 0);
        assert_eq!(base(), 0);
        // SAFETY: taking an interrupt off a machine that has none is
        // harmless: the register read is compiled out.
        assert_eq!(unsafe { acknowledge_next() }, None);
    }
}
