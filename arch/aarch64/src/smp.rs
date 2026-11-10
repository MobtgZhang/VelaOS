//! The AArch64 secondary-hart bring-up and the per-CPU identity.
//!
//! # How a second core starts
//!
//! AArch64 has no firmware interface for starting a hart the way SBI's HSM
//! extension does on RISC-V. QEMU's `virt` machine starts every core with
//! the same entry point, and each one asks the boot code which core it is:
//! the hardware identifier is `MPIDR_EL1.Aff0`, and the boot code maps that
//! onto a dense index.
//!
//! # The core identity
//!
//! `MPIDR_EL1.Aff0` is the core's identifier inside its cluster. The kernel
//! publishes the *index* it uses for the per-CPU arrays, and the hardware
//! identifier stays where it is: in the register.

use core::sync::atomic::{AtomicU32, Ordering};

/// The number of cores the kernel knows about.
///
/// The bound is the same one the other architecture crates use, so the
/// kernel's per-CPU bookkeeping is the same size everywhere.
pub const MAX_CPUS: usize = 256;

/// The index of this CPU into the per-CPU arrays.
static THIS_CPU_INDEX: AtomicU32 = AtomicU32::new(0);

/// The number of cores the boot code found.
static CORE_COUNT: AtomicU32 = AtomicU32::new(1);

/// The index of this CPU.
pub fn current_cpu_index() -> usize {
    THIS_CPU_INDEX.load(Ordering::Acquire) as usize
}

/// Publish the index of this CPU.
pub fn set_current_cpu_index(index: usize) {
    THIS_CPU_INDEX.store(index as u32, Ordering::Release);
}

/// The number of cores the machine has.
pub fn core_count() -> u32 {
    CORE_COUNT.load(Ordering::Acquire)
}

/// Record the number of cores the machine has.
pub fn set_core_count(count: u32) {
    CORE_COUNT.store(count.clamp(1, MAX_CPUS as u32), Ordering::Release);
}

/// The hardware identifier of this core, `MPIDR_EL1.Aff0`.
pub fn hardware_id() -> u32 {
    if crate::regs::ON_BARE_METAL {
        return (crate::regs::read_mpidr() & 0xFF) as u32;
    }
    0
}

/// Park this hart until an interrupt wakes it.
///
/// A secondary hart has nothing to do until the kernel gives it work, so it
/// waits in the idle loop: interrupts are masked, and `wfi` parks the hart
/// until one arrives.
pub fn park() -> ! {
    loop {
        crate::regs::wfi();
    }
}

#[cfg(test)]
extern crate std;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_index_round_trips() {
        set_current_cpu_index(3);
        assert_eq!(current_cpu_index(), 3);
        set_current_cpu_index(0);
        assert_eq!(current_cpu_index(), 0);
    }

    #[test]
    fn the_core_count_is_bounded() {
        set_core_count(2);
        assert_eq!(core_count(), 2);
        // A machine cannot have more cores than the kernel has slots for.
        set_core_count(MAX_CPUS as u32 + 10);
        assert_eq!(core_count(), MAX_CPUS as u32);
        set_core_count(1);
        assert_eq!(core_count(), 1);
    }
}
