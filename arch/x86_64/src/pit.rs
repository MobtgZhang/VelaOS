//! The 8254 PIT, and the TSC frequency it calibrates.
//!
//! # Why the PIT and not the APIC timer
//!
//! Every other clock in this kernel needs calibrating against some other
//! clock: the APIC timer's bus rate is not architecturally fixed, and the
//! TSC's rate is not either — calibrating one against the other would only
//! measure how consistent the guess was. The PIT's oscillator *is*
//! architecturally fixed at 1193182 Hz, which makes it the one clock that
//! needs no calibration, and so the one clock that can end the chain.
//!
//! # How the measurement works
//!
//! Channel 2 is programmed in mode 0 — count down once from a known value
//! and stop — and the countdown is polled by latching and reading the count
//! back. The TSC is read at the start and at a known number of counts later;
//! the ratio is the TSC's frequency. The window is about 27 ms: long enough
//! that the TSC read latency is noise, short enough that boot does not
//! stall.
//!
//! Channel 2 rather than channel 0 because channel 0's output is wired to
//! the PIC's IRQ0, and a mode-0 countdown on it would raise an interrupt no
//! handler is expecting. Channel 2's output drives the speaker instead, and
//! the speaker's gate is switched off while the count runs so the machine
//! stays silent.
//!
//! # Safety
//!
//! Everything here touches I/O ports, so every function is `unsafe` and the
//! caller promises the kernel owns the PIT.

use crate::regs;

/// The PIT's oscillator frequency, in Hz. Architecturally fixed.
const PIT_HZ: u64 = 1_193_182;

/// Channel 2's data port.
const PORT_DATA2: u16 = 0x42;

/// The command register.
const PORT_CONTROL: u16 = 0x43;

/// The port 0x61 gate register: bit 0 is channel 2's countdown gate, bit 1
/// the speaker's output gate.
const PORT_GATE: u16 = 0x61;

/// Latch channel 2's down-count for reading. The latch command is the
/// channel number in the top two bits and nothing else set.
const LATCH_CH2: u8 = 0b1000_0000;

/// Program channel 2: lobyte+hibyte access, mode 0 (count down once, hold at
/// zero), binary counting. Bits 7-6 name the channel, 5-4 the access mode,
/// 3-1 the mode, bit 0 the counting base.
const CONTROL_MODE0_CH2: u8 = 0b1011_0000;

/// Read channel 2's latched down-count.
///
/// # Safety
///
/// The kernel owns the PIT.
unsafe fn read_count() -> u32 {
    // SAFETY: the PIT's command and data ports, which this driver owns.
    unsafe {
        regs::outb(PORT_CONTROL, LATCH_CH2);
        let low = regs::inb(PORT_DATA2) as u32;
        let high = regs::inb(PORT_DATA2) as u32;
        (high << 8) | low
    }
}

/// Calibrate the TSC against the PIT.
///
/// Returns the TSC's frequency in Hz, or `None` when the machine has no PIT
/// or the countdown never moved — a caller with no calibrated TSC falls back
/// to whatever coarser clock it has, which is better than a wrong number
/// that looks precise.
///
/// # Safety
///
/// The kernel owns the PIT.
pub unsafe fn calibrate_tsc() -> Option<u64> {
    // SAFETY: the gate register belongs to this driver.
    let gate = unsafe { regs::inb(PORT_GATE) };
    // Gate channel 2 on, speaker off: the countdown runs, the machine
    // stays silent.
    // SAFETY: as above.
    unsafe { regs::outb(PORT_GATE, (gate | 0b0001) & !0b0010) };

    // SAFETY: the PIT's command and data ports.
    unsafe {
        regs::outb(PORT_CONTROL, CONTROL_MODE0_CH2);
        // Count down from 0xFFFF: about 55 ms. The measurement stops at the
        // halfway mark, so the window is about 27 ms.
        regs::outb(PORT_DATA2, 0xFF);
        regs::outb(PORT_DATA2, 0xFF);
    }

    let start_tsc = regs::tsc();
    let start_count = unsafe { read_count() };
    if start_count == 0 {
        // The countdown never started: no PIT, or a machine that does not
        // gate it the way the datasheet says.
        // SAFETY: the gate register.
        unsafe { regs::outb(PORT_GATE, gate) };
        return None;
    }

    // Poll until the count has fallen through a known number of PIT cycles:
    // half the initial count, which keeps the arithmetic exact and the
    // window comfortably inside the countdown's 55 ms.
    let target = start_count - 32768;
    let mut count = start_count;
    while count > target {
        count = unsafe { read_count() };
    }
    let end_tsc = regs::tsc();
    let counts = start_count - count;

    // SAFETY: the gate register.
    unsafe { regs::outb(PORT_GATE, gate) };

    if counts == 0 {
        return None;
    }
    let cycles = end_tsc.saturating_sub(start_tsc);
    // Cycles per PIT count, times the PIT's fixed rate: the TSC's Hz.
    let hz = cycles.checked_mul(PIT_HZ)?.checked_div(counts as u64)?;
    (hz > 1_000_000).then_some(hz)
}
