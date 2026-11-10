//! The LoongArch64 secondary-hart bring-up and the per-CPU identity.
//!
//! # How a second core starts
//!
//! LoongArch has no firmware interface for starting a hart in the way SBI's
//! HSM extension does on RISC-V. The board's boot firmware starts the
//! first hart with every core already enabled, and the boot code finds the
//! others through the platform's own tables. The kernel's job is therefore
//! small: know how many cores there are, know which one this is, and give
//! the others somewhere to wait.
//!
//! # The core identity
//!
//! The core number is not in a register a hart can read on its own — it
//! comes from the boot tables, which the firmware hands to the first hart.
//! The kernel therefore keeps the number it was given, and the boot code
//! publishes it before the secondary cores start.

use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

/// The number of cores the kernel knows about.
///
/// The bound is the same one the x86-64 crate uses, so the kernel's
/// per-CPU bookkeeping is the same size on every architecture.
pub const MAX_CPUS: usize = 256;

/// The core number of the hart that is running, cached per hart.
static THIS_CORE: AtomicU32 = AtomicU32::new(0);

/// The number of cores the boot code found.
static CORE_COUNT: AtomicU32 = AtomicU32::new(1);

/// The core's frequency in hertz, as the timer reports it.
static CORE_FREQUENCY: AtomicU64 = AtomicU64::new(0);

/// The core number of this hart.
pub fn current_core_id() -> u32 {
    THIS_CORE.load(Ordering::Acquire)
}

/// Set this hart's core number.
///
/// Called by the boot code, which is the only thing that knows the number:
/// it comes from the firmware's tables, not from a register the hart can
/// read.
pub fn set_current_core_id(core: u32) {
    THIS_CORE.store(core, Ordering::Release);
}

/// The number of cores the machine has.
pub fn core_count() -> u32 {
    CORE_COUNT.load(Ordering::Acquire)
}

/// Record the number of cores the machine has.
pub fn set_core_count(count: u32) {
    CORE_COUNT.store(count.clamp(1, MAX_CPUS as u32), Ordering::Release);
}

/// The core's frequency in hertz.
pub fn core_frequency() -> u64 {
    CORE_FREQUENCY.load(Ordering::Acquire)
}

/// Record the core's frequency.
pub fn set_core_frequency(hz: u64) {
    CORE_FREQUENCY.store(hz, Ordering::Release);
}

/// Park this hart until an interrupt wakes it.
///
/// A secondary hart has nothing to do until the kernel gives it work, so
/// it waits in the idle loop: interrupts are masked, and the `idle`
/// instruction parks the hart until one arrives.
pub fn park() -> ! {
    loop {
        // SAFETY: `idle` is defined in PLV 0 and only waits for an
        // interrupt; it has no side effect beyond waiting.
        unsafe { crate::regs::idle() };
    }
}

#[cfg(test)]
extern crate std;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_core_identity_round_trips() {
        set_current_core_id(3);
        assert_eq!(current_core_id(), 3);
        set_current_core_id(0);
        assert_eq!(current_core_id(), 0);
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
