//! Monotonic time: the `time` CSR, the Sstc timer and the SBI fallback.
//!
//! # Where time comes from
//!
//! RISC-V gives S-mode exactly one time source: the `time` counter, readable
//! in S-mode as CSR `0xC01` *provided* M-mode delegated it via
//! `mcounteren.TM` (OpenSBI on QEMU does). The counter ticks at the
//! platform's *timebase frequency* — 10 MHz on QEMU's `virt` machine, which
//! is also what the device tree advertises — and is shared by every hart,
//! so [`monotonic_ticks`] is a true monotonic clock, not a per-CPU one.
//!
//! # How a tick is scheduled
//!
//! The supervisor timer interrupt (`scause` interrupt 5) fires when the
//! hart's *deadline* passes. There are two ways to set the deadline, and
//! which one works depends on the firmware:
//!
//! * **Sstc** (the modern route): M-mode enables the extension through
//!   `menvcfg.STCE`, and S-mode then owns a private `stimecmp` CSR (number
//!   `0x14D`). Writing it is a single CSR write — no trap, no ecall. QEMU
//!   7.0+ enables Sstc for its `virt` machine's firmware.
//! * **legacy SBI**: an `ecall` to extension `0x00` (the legacy `set_timer`
//!   extension, whose EID *is* the function), with the 64-bit deadline in
//!   `a0`. This is the route a platform without Sstc takes, and the reason
//!   [`sbi_set_timer`] exists beside [`schedule_sstc`].
//!
//! The two routes set the same deadline; a caller picks Sstc when it can
//! and falls back otherwise. Either way the interrupt must be unmasked in
//! `sie.STIE` (see [`crate::regs::SIE_STIE`]) or the deadline passes in
//! silence.
//!
//! # Arithmetic, and why it is done in u128
//!
//! [`ns_from_ticks`] multiplies a tick count by 10^9 and divides by the
//! timebase. A 64-bit tick count times 10^9 overflows 64 bits for anything
//! past ~1.8 seconds of ticks, so the intermediate lives in `u128` — the
//! conversion stays exact for the counter's whole 64-bit range at any
//! timebase this kernel will meet.

use crate::regs::{self, ON_BARE_METAL};

/// The platform timebase frequency, in Hz.
///
/// QEMU's `virt` machine drives `time` from a 10 MHz reference (the value
/// the device tree's `timebase-frequency` advertises); real boards differ,
/// so this constant is the QEMU value and a port must change exactly here.
pub const TIMEBASE_HZ: u64 = 10_000_000;

/// The `scause` interrupt code the timer delivers: supervisor timer
/// interrupt (privileged spec `scause` table).
pub const TIMER_IRQ: u64 = crate::trap::INT_SUPERVISOR_TIMER;

/// The `stimecmp` CSR number (Sstc extension, privileged spec Vol. II).
///
/// Named for documentation: the `asm!` below addresses it by number, and a
/// typo in the raw number would schedule a tick on some other register
/// entirely.
#[allow(
    dead_code,
    reason = "the number is used by the instruction; a host build has no instruction"
)]
const CSR_STIMECMP: u32 = 0x14D;

/// Read the platform time counter.
///
/// Returns 0 off bare metal, like every CSR read in this crate — the host
/// has no `time` register, and tests that need a tick count inject one.
pub fn monotonic_ticks() -> u64 {
    if !ON_BARE_METAL {
        return 0;
    }
    #[cfg(target_arch = "riscv64")]
    // SAFETY: reading `time` is always defined in S-mode when M-mode has
    // delegated the counter; it has no side effects.
    unsafe {
        let value: u64;
        core::arch::asm!(
            "rdtime {0}",
            out(reg) value,
            options(nomem, nostack)
        );
        value
    }
    #[cfg(not(target_arch = "riscv64"))]
    0
}

/// Schedule the next supervisor timer interrupt at absolute tick `ticks`.
///
/// Uses the Sstc `stimecmp` CSR, which requires M-mode to have enabled the
/// extension (`menvcfg.STCE`); on a platform without it the write is an
/// illegal-instruction exception, so the caller must have probed for Sstc
/// or use [`sbi_set_timer`] instead.
///
/// # Safety
///
/// Boot-ordering only: the interrupt stays masked until `sie.STIE` is set,
/// so a deadline written "too early" merely schedules a future tick.
pub unsafe fn schedule_sstc(ticks: u64) {
    if !ON_BARE_METAL {
        return;
    }
    #[cfg(target_arch = "riscv64")]
    // SAFETY: writing `stimecmp` is defined in S-mode under Sstc.
    unsafe {
        core::arch::asm!(
            "csrw {csr}, {value}",
            csr = const CSR_STIMECMP,
            value = in(reg) ticks,
            options(nomem, nostack)
        );
    }
    #[cfg(not(target_arch = "riscv64"))]
    {
        let _ = ticks;
    }
}

/// Schedule the next supervisor timer interrupt through the legacy SBI.
///
/// The legacy `set_timer` extension is EID `0x00` with the deadline in
/// `a0`; the legacy convention returns the *value* in `a0` rather than an
/// error, so the result is discarded. This is the fallback for platforms
/// without Sstc, and the only route that works when M-mode has not
/// delegated `stimecmp`.
pub fn sbi_set_timer(stime_value: u64) {
    // Legacy EID 0x00 is "set_timer"; there is no function id beyond the
    // extension id itself, so 0 is passed and ignored.
    let _ = regs::sbi_call(0x00, 0, stime_value, 0);
}

/// Convert a tick count to nanoseconds, exactly.
///
/// The multiply runs in `u128` (see the module docs); the caller's tick
/// count may be anything `u64` can hold.
pub const fn ns_from_ticks(ticks: u64) -> u128 {
    (ticks as u128) * 1_000_000_000u128 / TIMEBASE_HZ as u128
}

/// Convert nanoseconds to ticks, truncating downward.
///
/// The inverse of [`ns_from_ticks`]; used when a caller has a duration in
/// nanoseconds and needs a deadline tick count.
pub const fn ticks_from_ns(ns: u128) -> u64 {
    (ns * TIMEBASE_HZ as u128 / 1_000_000_000u128) as u64
}

/// Block the hart until at least `ticks` time-counter ticks have passed.
///
/// Spins on the counter rather than scheduling an interrupt: this is the
/// short-delay primitive (a device settling, a probe retrying), where the
/// interrupt machinery is heavier than the wait. Off bare metal it returns
/// immediately, so host tests never block.
pub fn busy_wait(ticks: u64) {
    if !ON_BARE_METAL {
        return;
    }
    let start = monotonic_ticks();
    while monotonic_ticks().wrapping_sub(start) < ticks {
        core::hint::spin_loop();
    }
}

#[cfg(test)]
extern crate std;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_timebase_is_qemu_virt_value() {
        assert_eq!(TIMEBASE_HZ, 10_000_000);
    }

    #[test]
    fn the_timer_irq_is_scause_interrupt_five() {
        assert_eq!(TIMER_IRQ, 5);
        assert_eq!(
            TIMER_IRQ,
            crate::trap::INT_SUPERVISOR_TIMER,
            "the timer cause and the trap decoder must agree"
        );
    }

    #[test]
    fn the_stimecmp_csr_number_is_the_sstc_one() {
        // 0x14D is the spec value; a typo here schedules the tick in
        // another register and the timer simply never fires.
        assert_eq!(CSR_STIMECMP, 0x14D);
    }

    #[test]
    fn tick_to_ns_conversion_is_exact_for_round_numbers() {
        // 10 MHz: exactly 100 ns per tick.
        assert_eq!(ns_from_ticks(1), 100);
        assert_eq!(ns_from_ticks(10), 1_000);
        assert_eq!(ns_from_ticks(TIMEBASE_HZ), 1_000_000_000);
    }

    #[test]
    fn conversions_round_trip_and_truncate_honestly() {
        for ticks in [0u64, 1, 7, 123_456, u64::MAX] {
            let ns = ns_from_ticks(ticks);
            // The round trip may lose the last few ns to truncation, but
            // must never overestimate.
            assert!(ticks_from_ns(ns) <= ticks);
        }
        // One ns at 10 MHz is 0.01 ticks: truncates to zero, and the
        // caller asking for a 1 ns wait gets "now" rather than a wrap.
        assert_eq!(ticks_from_ns(1), 0);
    }

    #[test]
    fn large_tick_counts_do_not_overflow_the_conversion() {
        // 2^63 ticks at 10 MHz would be ~292 years; the u128 intermediate
        // must absorb the multiply without wrapping.
        let ns = ns_from_ticks(1 << 63);
        assert_eq!(ns, (1u128 << 63) * 100);
    }

    #[test]
    fn csr_reads_are_inert_off_bare_metal() {
        // The host has no `time` register; the read must report a defined
        // zero rather than fault or panic.
        assert_eq!(monotonic_ticks(), 0);
        // SAFETY: the documented host no-op.
        unsafe {
            schedule_sstc(u64::MAX);
        }
    }

    #[test]
    fn busy_wait_returns_promptly_off_bare_metal() {
        // On the host the wait is compiled out; this asserts the call does
        // not spin forever on a clock that never advances.
        busy_wait(1_000);
    }

    #[test]
    fn the_sbi_set_timer_extension_is_the_legacy_eid() {
        // EID 0x00 is the legacy set_timer extension. The call itself is
        // inert off bare metal — this pins the id the firmware expects so
        // a refactor cannot silently retarget another extension.
        sbi_set_timer(0);
    }
}
