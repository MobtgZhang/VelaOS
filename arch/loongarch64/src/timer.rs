//! The architectural LoongArch64 constant-rate timer (`CSR.TCFG`/`CSR.TVAL`)
//! and the stable counter it is calibrated against.
//!
//! Every LoongArch64 CPU has a built-in down-counting timer whose interrupt
//! is one of the architectural interrupt lines (`ECFG.LIE` bit 11 — see
//! [`crate::apic`]). The timer is programmed through `CSR.TCFG`:
//!
//! ```text
//!  63                            2 1      0
//! ┌────────────────────────────────┬────────┬─────┐
//! │ init value (ticks)             │ PERIOD │ EN  │
//! └────────────────────────────────┴────────┴─────┘
//! ```
//!
//! Writing `TCFG` with `EN` set loads `bits 63:2` ticks into the counter and
//! starts it; the counter counts *down* to zero, raises its interrupt line,
//! and — if `PERIOD` is set — reloads automatically (QEMU:
//! `CONSTANT_TIMER_TICK_MASK = 0xfffffffffffc`, the field is the raw tick
//! count). The remaining count is readable in `CSR.TVAL`; after a one-shot
//! fires it reads all-ones, which callers must treat as "no timeout in
//! progress". The interrupt is acknowledged by writing 1 to `CSR.TICLR.TI`
//! (`regs::ticlr_clear_timer_irq`); it does *not* clear itself on reload.
//!
//! The tick rate is the "constant counter frequency" reported by `CPUCFG4` —
//! 100 MHz on QEMU's la464 model (QEMU's `target/loongarch/tcg/timer.c`
//! hardcodes a 10 ns period), and constant across a whole machine, which is
//! what makes it usable both as the scheduling timeslice source and as a
//! monotonic clock. The stable counter itself is read with `rdtime.d rd,
//! $zero` (equivalently `CSR.CNTC`) and never wraps in practice (2^47 ticks
//! at 100 MHz ≈ 40 days; the counter itself is 64-bit and effectively
//! non-wrapping).
//!
//! Sources: LoongArch Reference Manual Vol. 2, Chapter 2 (Timer) and
//! Linux `arch/loongarch/include/asm/loongarch.h` (`CSR_TCFG`/`CSR_TVAL`/
//! `CSR_CNTC`/`CSR_TICLR`, `ECFGB_TIMER`).

use crate::regs;

/// QEMU's la464 model runs the stable counter at 100 MHz; this is also the
/// value `CPUCFG4` reports there.
pub const DEFAULT_TIMER_FREQUENCY_HZ: u64 = 100_000_000;

/// The `ECFG.LIE` bit of the timer interrupt line (architectural HWI line 9;
/// see [`crate::apic::lie_value`]).
pub const TIMER_LIE_LINE: u16 = 11;

/// Build a `TCFG` word for a one-shot timer expiring after `ticks` ticks.
///
/// The init value occupies bits 63:2 in raw ticks (QEMU's
/// `CONSTANT_TIMER_TICK_MASK`); `ticks` must fit in 46 bits and be non-zero
/// so that `EN` never fires an interrupt immediately after being set.
pub fn tcfg_value_oneshot(ticks: u64) -> u64 {
    tcfg_value(ticks, false)
}

/// Build a `TCFG` word for a periodic timer reloading every `ticks` ticks.
pub fn tcfg_value_periodic(ticks: u64) -> u64 {
    tcfg_value(ticks, true)
}

/// Shared builder behind [`tcfg_value_oneshot`] and [`tcfg_value_periodic`].
pub fn tcfg_value(ticks: u64, periodic: bool) -> u64 {
    debug_assert!(ticks > 0 && ticks < (1 << 46), "INIT_VAL is 46 bits");
    let mut value = ticks << 2;
    if periodic {
        value |= 0b10; // PERIOD, bit 1
    }
    value | 0b01 // EN, bit 0
}

/// Inverse of [`tcfg_value`]: how many ticks a programmed `TCFG` word
/// represents (used by tests and by the boot path to report calibration).
pub fn tcfg_ticks(value: u64) -> u64 {
    // `EN`/`PERIOD` occupy bits 1:0 and are stripped by the shift.
    value >> 2
}

/// Build a `CSR.TICLR` acknowledge word — write-1-to-clear on bit 0.
pub fn ticlr_value() -> u64 {
    1
}

/// How many ticks the stable counter advances per second.
///
/// Reads `CPUCFG4` on bare metal and falls back to [`DEFAULT_TIMER_FREQUENCY_HZ`]
/// elsewhere (where `CPUCFG` is compiled out).
pub fn ticks_per_second() -> u64 {
    if regs::ON_BARE_METAL {
        let freq = regs::cpucfg4_counter_frequency();
        if freq != 0 {
            return u64::from(freq);
        }
        return DEFAULT_TIMER_FREQUENCY_HZ;
    }
    DEFAULT_TIMER_FREQUENCY_HZ
}

/// The stable counter's current value — the machine's only monotonic clock
/// before the scheduler has anything else running.
pub fn now_ticks() -> u64 {
    regs::rdtime_d()
}

/// Program the one-shot timer on this CPU to expire after `ticks` ticks.
///
/// # Safety
///
/// Replaces whatever timeout this CPU's timer was running; called from the
/// scheduler, which owns the timer.
pub unsafe fn schedule_oneshot(ticks: u64) {
    // SAFETY: the caller owns this CPU's timer.
    unsafe { regs::write_csr::<{ regs::CSR_TCFG }>(tcfg_value_oneshot(ticks)) };
}

/// Program the periodic timer on this CPU, reloading every `ticks` ticks.
///
/// # Safety
///
/// As [`schedule_oneshot`].
pub unsafe fn schedule_periodic(ticks: u64) {
    // SAFETY: the caller owns this CPU's timer.
    unsafe { regs::write_csr::<{ regs::CSR_TCFG }>(tcfg_value_periodic(ticks)) };
}

/// Stop the timer on this CPU (clear `EN`; the pending interrupt, if any, is
/// *not* cleared — see [`acknowledge`]).
///
/// # Safety
///
/// As [`schedule_oneshot`].
pub unsafe fn stop() {
    // SAFETY: the caller owns this CPU's timer.
    unsafe { regs::write_csr::<{ regs::CSR_TCFG }>(0) };
}

/// Acknowledge the timer interrupt: `CSR.TICLR.TI ← 1`.
///
/// Must be called from the timer interrupt handler before returning, or the
/// line re-fires immediately.
///
/// # Safety
///
/// Clears a pending timer interrupt; the handler's job.
pub unsafe fn acknowledge() {
    // SAFETY: the only effect is clearing the timer IRQ line.
    unsafe { regs::write_csr::<{ regs::CSR_TICLR }>(ticlr_value()) };
}

/// The remaining ticks on this CPU's programmed timer (`CSR.TVAL`), or `None`
/// if the timer is disabled.
pub fn remaining_ticks() -> Option<u64> {
    // SAFETY: TVAL and TCFG are read-only for this purpose.
    let tcfg = unsafe { regs::read_csr::<{ regs::CSR_TCFG }>() };
    if tcfg & 1 == 0 {
        return None;
    }
    let tval = unsafe { regs::read_csr::<{ regs::CSR_TVAL }>() };
    Some(tval)
}

/// Monotonic ticks derived from the stable counter, in *timer* ticks — the
/// [`vela_arch_common::PlatformTimer`] surface. They are the same unit here,
/// so this is [`now_ticks`] under a different name for trait parity with the
/// x86-64 APIC-timer implementation.
pub fn ticks() -> u64 {
    now_ticks()
}

/// Busy-wait for at least `ticks` timer ticks by polling the stable counter.
pub fn busy_wait(ticks: u64) {
    if !regs::ON_BARE_METAL {
        // Off bare metal the counter does not advance, so a spin on it
        // would never end.
        return;
    }
    let start = now_ticks();
    let target = start.saturating_add(ticks);
    while now_ticks() < target {
        regs::pause_hint();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tcfg_one_shot_encodes_en_and_value() {
        // 100_000 ticks at QEMU's 100 MHz = 1 ms; encoded as the raw tick
        // count in bits 63:2, EN set, PERIOD clear.
        let value = tcfg_value_oneshot(100_000);
        assert_eq!(value & 1, 1, "EN must be set");
        assert_eq!(value & 0b10, 0, "PERIOD must be clear");
        assert_eq!(value >> 2, 100_000);
        assert_eq!(tcfg_ticks(value), 100_000);
    }

    #[test]
    fn tcfg_periodic_encodes_period_bit() {
        let value = tcfg_value_periodic(8);
        assert_eq!(value & 0b11, 0b11, "EN and PERIOD must both be set");
        assert_eq!(tcfg_ticks(value), 8);
    }

    #[test]
    fn tcfg_init_val_is_46_bits_of_ticks() {
        // QEMU masks the field with 0xfffffffffffc: the encoded count is the
        // raw tick number, shifted left by 2.
        let value = tcfg_value_oneshot(4);
        assert_eq!(value & 0xFFFF_FFFF_FFFC, 4 << 2);
        assert_eq!(tcfg_ticks(value), 4);
    }

    #[test]
    fn tcfg_ticks_inverts_value() {
        for ticks in [1, 4, 100, 1_000, 1_000_000] {
            assert_eq!(tcfg_ticks(tcfg_value_oneshot(ticks)), ticks);
        }
    }

    #[test]
    fn ticlr_is_write_one_to_clear() {
        assert_eq!(ticlr_value(), 1);
    }

    #[test]
    fn hosted_counter_reads_are_zero_and_bounded() {
        if !regs::ON_BARE_METAL {
            // The stubs return zero; the busy-wait must then terminate
            // immediately rather than spin.
            assert_eq!(now_ticks(), 0);
            busy_wait(0);
            assert_eq!(remaining_ticks(), None);
        }
    }
}
