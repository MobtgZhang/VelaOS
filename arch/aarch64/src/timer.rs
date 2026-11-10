//! The AArch64 generic timers.
//!
//! # The two counters
//!
//! AArch64 has two counters:
//!
//! * the **physical** counter `CNTPCT_EL0`, which counts at a fixed rate
//!   given by `CNTFRQ_EL0` and never stops — it is the monotonic clock;
//! * the **virtual** timer `CNTVCT_EL0` / `CNTV_*`, which runs at the
//!   physical counter's rate but can be offset and armed, and which raises
//!   interrupt PPI 27 when it expires.
//!
//! The kernel uses the physical counter for time and the virtual timer for
//! the tick.
//!
//! # The frequency
//!
//! `CNTFRQ_EL0` says how many times the counter ticks per second. QEMU's
//! `virt` model reports 62.5 MHz (or 100 MHz on newer models); the number
//! is read from the hardware rather than assumed, because a kernel that
//! assumed the wrong frequency would run its delays at the wrong speed.

use crate::regs;

/// The virtual timer's interrupt line.
///
/// PPI 27 is the virtual timer on AArch64.
pub const TIMER_IRQ: u32 = 27;

/// The physical counter's frequency, in hertz.
///
/// Returns zero off bare metal, where no system register can be read.
pub fn frequency() -> u64 {
    if !regs::ON_BARE_METAL {
        return 0;
    }
    // SAFETY: `CNTFRQ_EL0` is readable at EL0 and has no side effects.
    unsafe { regs::read_sysreg(regs::names::CNTFRQ_EL0) }
}

/// The physical counter's current value.
///
/// This is the monotonic clock: it counts up and never stops.
pub fn now_ticks() -> u64 {
    if !regs::ON_BARE_METAL {
        return 0;
    }
    // SAFETY: `CNTPCT_EL0` is readable at EL0 and has no side effects.
    unsafe { regs::read_sysreg("cntpct_el0") }
}

/// The virtual timer's current value.
pub fn virtual_ticks() -> u64 {
    if !regs::ON_BARE_METAL {
        return 0;
    }
    // SAFETY: `CNTVCT_EL0` is readable at EL0 and has no side effects.
    unsafe { regs::read_sysreg("cntvct_el0") }
}

/// Arm the virtual timer to expire after `ticks`.
///
/// The timer is armed against the *physical* counter (`CNTV_CTL_EL0` with
/// `IC` clear counts physical ticks), so the deadline is expressed in the
/// same units as [`now_ticks`].
///
/// # Safety
///
/// The caller must own the timer: arming it twice without the first expiry
/// having been acknowledged loses the first deadline.
pub unsafe fn schedule(ticks: u64) {
    if !regs::ON_BARE_METAL {
        return;
    }
    let now = now_ticks();
    // SAFETY: writing the timer registers is defined at EL1; the caller owns
    // the timer.
    unsafe {
        // The mask register first, so no expiry can slip through between
        // the two writes.
        regs::write_sysreg("cntv_mask_el0", 1);
        regs::write_sysreg("cntv_tval_el0", now.wrapping_add(ticks));
        regs::write_sysreg("cntv_ctl_el0", 1);
    }
}

/// Disarm the virtual timer.
///
/// # Safety
///
/// The caller must own the timer: disarming it twice without the first
/// expiry having been acknowledged loses the deadline.
pub unsafe fn cancel() {
    if !regs::ON_BARE_METAL {
        return;
    }
    // SAFETY: as [`schedule`].
    unsafe {
        regs::write_sysreg("cntv_ctl_el0", 0);
        regs::write_sysreg("cntv_mask_el0", 1);
    }
}

/// Acknowledge a virtual-timer expiry.
pub fn acknowledge() {
    if !regs::ON_BARE_METAL {
        return;
    }
    // SAFETY: reading the timer status register has no side effect; the
    // acknowledgement is the write of the status register, which the
    // architecture defines as writing the value read.
    unsafe {
        let status = regs::read_sysreg("cntv_ctl_el0");
        regs::write_sysreg("cntv_ctl_el0", status & !(1 << 1));
    }
}

/// The number of ticks in a duration, given the counter's frequency.
pub const fn ticks_for(nanos: u64, frequency: u64) -> u64 {
    if frequency == 0 {
        return 0;
    }
    // The multiplication is done in halves so that a long duration does not
    // overflow before it is divided.
    let whole = nanos / 1_000_000_000;
    let remainder = nanos % 1_000_000_000;
    whole
        .saturating_mul(frequency)
        .saturating_add(remainder.saturating_mul(frequency) / 1_000_000_000)
}

/// Spin for `ticks` counter ticks.
pub fn busy_wait(ticks: u64) {
    if !regs::ON_BARE_METAL {
        // Off bare metal the counter does not advance, so a spin on it would
        // never end.
        return;
    }
    let start = now_ticks();
    let target = start.wrapping_add(ticks);
    while now_ticks() < target {
        core::hint::spin_loop();
    }
}

#[cfg(test)]
extern crate std;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_counter_is_inert_off_bare_metal() {
        assert_eq!(frequency(), 0);
        assert_eq!(now_ticks(), 0);
        assert_eq!(virtual_ticks(), 0);
        busy_wait(1000);
    }

    #[test]
    fn the_virtual_timer_raises_ppi_27() {
        // The virtual timer is PPI 27 on AArch64.
        assert_eq!(TIMER_IRQ, 27);
    }

    #[test]
    fn the_tick_count_follows_the_frequency() {
        // A millisecond at 100 MHz is 100 000 ticks.
        assert_eq!(ticks_for(1_000_000, 100_000_000), 100_000);
        // A second at 62.5 MHz.
        assert_eq!(ticks_for(1_000_000_000, 62_500_000), 62_500_000);
        // No frequency, no ticks.
        assert_eq!(ticks_for(1_000_000, 0), 0);
    }

    #[test]
    fn a_long_duration_does_not_overflow() {
        // A duration so long that the multiplication would overflow must
        // saturate rather than wrap.
        let ticks = ticks_for(u64::MAX, 1_000_000_000);
        assert_eq!(ticks, u64::MAX);
    }
}
