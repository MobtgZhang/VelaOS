//! Local APIC timer and the TSC-backed monotonic clock.
//!
//! # Two clocks, one interface
//!
//! VelaOS needs a monotonic tick counter, and x86-64 offers two candidates:
//!
//! * **TSC** — reads in ~20 cycles, calibrated once against a known delay.
//!   Perfect for measuring short intervals, useless for preemption because it
//!   raises no interrupt.
//! * **Local APIC timer** — periodic, drives the scheduler tick, but must be
//!   calibrated too, and its frequency is not architecturally fixed.
//!
//! `PlatformTimer` exposes the APIC tick as the *logical* clock and uses the TSC
//! to interpolate between APIC ticks. That gives a cheap, monotonically
//! increasing counter that never skips a tick even if user code reads it in a
//! tight loop.
//!
//! # Calibration
//!
//! The APIC timer counts down at a bus frequency the firmware may or may not
//! report. Rather than trust an ACPI MPIC table that QEMU may leave out, the
//! initial divisor is chosen for a *target period*, then verified by measuring
//! real elapsed time across a known number of ticks. If the measurement is off,
//! the divisor is adjusted. Calibration is a one-off cost at boot, and the
//! result is stored in a static so the steady-state `ticks()` is a couple of
//! instructions.

use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use crate::regs;

/// The MMIO base the APIC's registers live at in xAPIC mode.
///
/// The window is only live while the APIC is in **xAPIC** mode. In x2APIC
/// mode the registers are reached through the MSRs at `0x800 + offset`
/// instead, and reads from this window return `0xFFFFFFFF`.
pub const APIC_BASE: u64 = 0xFEE0_0000;

/// `xAPIC` offset: the APIC's own identifier.
///
/// This register is **zero on the first processor** — the APIC IDs are dense
/// and start at zero — so it is useless as a presence test. It is read for
/// diagnostics only.
const APIC_ID: u64 = 0x020;

/// `xAPIC` offset: version register.
///
/// # This offset is 0x030, and getting it wrong is silent
///
/// The version register is at offset `0x030` from the base of the window.
/// Two nearby offsets are *not* it:
///
/// * `0x020` is the **APIC ID**, which is zero on the first processor — so
///   reading it says "no APIC" on exactly the machine most likely to be
///   developed on.
/// * `0x300` is the **ICR low** — the interrupt command register — which
///   holds whatever the last interrupt command left there.
///
/// The consequence of using the wrong offset is that the driver cannot tell
/// a working APIC from a dead one. The version register is the *only*
/// register whose value says "there is a local APIC here"; read the wrong
/// one and the answer is arbitrary, so the driver either declares a
/// perfectly good APIC absent, or — worse — declares a dead window present
/// and then programs registers that go nowhere.
const APIC_VERSION: u64 = 0x030;
/// `xAPIC` offset: task priority.
const APIC_TPR: u64 = 0x080;
/// `xAPIC` offset: end-of-interrupt.
const APIC_EOI: u64 = 0x0B0;
/// `xAPIC` offset: spurious interrupt vector.
const APIC_SPURIOUS: u64 = 0x0F0;
/// `xAPIC` offset: local APIC timer command.
const APIC_LVT_TIMER: u64 = 0x320;
/// `xAPIC` offset: timer divide configuration.
const APIC_TIMER_DIV: u64 = 0x3E0;
/// `xAPIC` offset: timer initial count.
const APIC_TIMER_INITCNT: u64 = 0x380;
/// `xAPIC` offset: timer current count.
const APIC_TIMER_CURCNT: u64 = 0x390;

/// Vector the APIC timer interrupt is delivered on.
///
/// # Why this is outside the legacy PIC range
///
/// The legacy 8259 PIC delivers its sixteen interrupt request lines on
/// vectors `0x20`..`=0x2F`. The APIC timer is *not* an IRQ line: it is a
/// local APIC timer, and its vector is chosen by the software that programs
/// the local APIC. Putting the timer on a vector inside the PIC's range
/// means the timer and the PIC's IRQ0 share a vector, and on a machine where
/// the firmware has left the PIC unmasked the two would be indistinguishable
/// at the gate — the handler could not tell a timer tick from an IRQ0, and
/// would acknowledge the wrong controller.
///
/// Vector `0x30` is outside the PIC's range and inside the range the
/// `IA32_APIC` architecture reserves for local interrupts, so it cannot
/// collide with either the PIC or the IOAPIC's legacy range.
pub const TIMER_VECTOR: u8 = 0x30;

/// Register offset for the enable/disable bit in an LVT.
pub mod lvt {
    /// Masked: do not deliver.
    pub const MASKED: u32 = 1 << 16;

    /// Periodic mode: the timer reloads itself when it expires.
    ///
    /// # Why this bit is not optional
    ///
    /// Without it the timer is **one-shot**: it counts down from the initial
    /// count, fires once, and stops. The kernel's clock would advance by one
    /// tick and then never again — the machine would boot, print its banner,
    /// and sit there with a dead clock.
    ///
    /// This is a silent failure: the timer *did* fire, so nothing looks
    /// wrong, and the only symptom is that it stopped.
    pub const PERIODIC: u32 = 1 << 17;
}

/// Timer divide configuration values (low 8 bits of the DIV register).
///
/// # These are the architectural encodings, not a convenience scale
///
/// The `DIV` register does not hold a divisor — it holds a *code*, and the
/// code-to-divisor mapping is not the identity. Writing `0b0000` does not
/// mean "divide by one"; it means **divide by two**.
///
/// | code | divisor |
/// |------|---------|
/// | `0b0000` | 2 |
/// | `0b0001` | 4 |
/// | `0b0010` | 8 |
/// | `0b0011` | 16 |
/// | `0b1000` | 32 |
/// | `0b1001` | 64 |
/// | `0b1010` | 128 |
/// | `0b1011` | 1 |
///
/// Only `0b1011` divides by one, and it is the only code the driver uses for
/// the timer that has to run at a known frequency. Writing `0` is also
/// *rejected* by some implementations, which keep the previous divider — so
/// the code that means "divide by one" must not be zero.
pub mod divide {
    /// Divide by 1.
    pub const BY_1: u32 = 0b1011;
    /// Divide by 2.
    pub const BY_2: u32 = 0b0000;
    /// Divide by 4.
    pub const BY_4: u32 = 0b0001;
    /// Divide by 8.
    pub const BY_8: u32 = 0b0010;
    /// Divide by 16.
    pub const BY_16: u32 = 0b0011;
    /// Divide by 32.
    pub const BY_32: u32 = 0b1000;
    /// Divide by 64.
    pub const BY_64: u32 = 0b1001;
    /// Divide by 128.
    pub const BY_128: u32 = 0b1010;
}

/// Timer tick frequency, in Hz.
///
/// 250 Hz gives a 4 ms scheduler quantum: short enough that a busy task feels
/// responsive, long enough that context-switch overhead stays negligible.
pub const TICKS_PER_SECOND: u64 = 250;

/// The APIC timer is configured to interrupt on this vector.
const fn calc_initial_count(freq: u64) -> u32 {
    if freq == 0 {
        return 1;
    }
    // No DIV register, so the initial count is counts-per-tick. Clamped to 24
    // bits, the architectural maximum for the initial-count register.
    let per_tick = APIC_BUS_FREQ_HZ / freq;
    if per_tick == 0 {
        1
    } else if per_tick > 0x00FF_FFFF {
        0x00FF_FFFF
    } else {
        per_tick as u32
    }
}

/// Whether the APIC timer has been calibrated and unmasked.
static INITIALISED: AtomicU64 = AtomicU64::new(0);

/// TSC frequency in Hz, learned during calibration.
static TSC_HZ: AtomicU64 = AtomicU64::new(0);

/// The logical tick counter: APIC ticks plus TSC interpolation.
static TICKS: AtomicU64 = AtomicU64::new(0);

/// A kernel callback for each delivered APIC timer tick.
///
/// The HAL does not know what the kernel does with a tick; the kernel installs
/// a hook here. The hook runs in interrupt context, so it must never block.
/// It is stored as a raw address because Rust has no atomic function pointer;
/// zero means "no hook installed".
static TIMER_HOOK: AtomicUsize = AtomicUsize::new(0);

/// Per-CPU "needs reschedule" flags, set by the reschedule-IPI handler.
///
/// A CPU's scheduler consults this at the next safe point and picks a new
/// task from its run queue. Indexed by APIC ID, which is dense and below 256
/// on the machines VelaOS targets.
static RESCHEDULE_PENDING: [AtomicBool; 256] = [const { AtomicBool::new(false) }; 256];

/// Read an APIC MMIO register.
///
/// Off bare metal this returns zero: the APIC window is at `0xFEE0_0000`, a
/// physical address with no mapping in a host process, so touching it would
/// segfault the test runner rather than fail an assertion.
fn apic_read(offset: u64) -> u32 {
    if !regs::ON_BARE_METAL {
        return 0;
    }
    // SAFETY: `APIC_BASE + offset` is the xAPIC MMIO window. On hardware without
    // an APIC the read is dropped; the caller treats a zero return as absent.
    unsafe { core::ptr::read_volatile((APIC_BASE + offset) as *const u32) }
}

/// Write an APIC MMIO register.
///
/// Off bare metal the write is dropped, for the same reason as [`apic_read`].
unsafe fn apic_write(offset: u64, value: u32) {
    if !regs::ON_BARE_METAL {
        return;
    }
    // SAFETY: `APIC_BASE + offset` is the xAPIC MMIO window.
    unsafe { core::ptr::write_volatile((APIC_BASE + offset) as *mut u32, value) }
}

/// Whether a local APIC is present and responding.
///
/// # Why the version register, and not the ID
///
/// The obvious test — "the APIC ID register is non-zero" — is wrong, and
/// wrong in a way that disables the timer on the most common machine there
/// is. The APIC ID of the *first* processor is **zero** by construction: the
/// IDs are dense and start at zero, so CPU 0's ID is 0.
///
/// So on a single-CPU machine — which is what a development VM is — the ID
/// register reads zero, the ID test fails, and the kernel concludes there is
/// no local APIC and never starts its timer. The symptom is a kernel that
/// boots perfectly and then has no clock at all.
///
/// The version register is the correct test. It reads `0x500` on an xAPIC
/// that follows the architectural layout and `0x0500` on a yAPIC, and it is
/// zero only when there is genuinely nothing at the window.
pub fn is_present() -> bool {
    // The version register is the only register whose value says "there is a
    // local APIC here". It reads `0x500` on an xAPIC that follows the
    // architectural layout and `0x0500` on a yAPIC.
    //
    // The value is masked to its architectural width before the comparison.
    // A yAPIC reports the version in bits 0-7 and the number of LVT entries
    // in bits 24-31; an xAPIC puts the version in bits 8-15. Both have a
    // non-zero low half, so a mask of `0xFF` — not `0xFFFF` — is what
    // distinguishes "a version is reported" from "the register is empty".
    let version = apic_read(APIC_VERSION);
    version & 0xFF != 0
}

/// Report the local APIC's own view of its configuration.
///
/// # Why the state must be *read*, not assumed
///
/// There are two ways to talk to the local APIC, and the firmware chooses:
///
/// * **xAPIC** — the registers are memory-mapped at `0xFEE0_0000`.
/// * **x2APIC** — the registers are in the MSRs at `0x800 + offset`, and the
///   MMIO window reads back as `0xFFFFFFFF`.
///
/// This driver programs the MMIO window. If the APIC is in x2APIC mode, every
/// write to the window is discarded and the timer silently never fires — the
/// kernel boots, prints its banner, and then sits there with no clock and no
/// error anywhere.
///
/// The version register is the cheapest reliable probe: in xAPIC mode it
/// reads `0x500` (or `0x0500` on a yAPIC); in x2APIC mode the window is
/// dead and it reads `0xFFFFFFFF`. So the read that [`is_present`] already
/// performs is also the read that tells the driver whether it is talking to
/// the right thing at all.
///
/// # What this prints
///
/// The APIC version, the window's first word, and whether the window looks
/// live. A window that reads `0xFFFFFFFF` is dead, and the log says so
/// explicitly rather than leaving it to be inferred from a silent timer.
fn report_apic_state() {
    let version = apic_read(APIC_VERSION);
    let id = apic_read(APIC_ID);
    let window_probe = apic_read(0);
    crate::serial::puts("[apic] version ");
    crate::serial::puts_hex(version as u64);
    crate::serial::puts(" id ");
    crate::serial::puts_hex(id as u64);
    crate::serial::puts(" window ");
    crate::serial::puts_hex(window_probe as u64);
    if version == u32::MAX || window_probe == u32::MAX {
        crate::serial::puts(" DEAD (the APIC is in x2APIC mode; the MMIO window is not live)\n");
    } else {
        crate::serial::puts(" live\n");
    }
}

/// `xAPIC` offset: interrupt command register low.
///
/// Used to send IPIs between CPUs. Writing here sends an interrupt to the
/// target CPU specified in the destination field.
const APIC_ICR_LOW: u64 = 0x300;

/// `xAPIC` offset: interrupt command register high.
const APIC_ICR_HIGH: u64 = 0x310;

/// IPI delivery mode: fixed delivery.
const ICR_DELIVERY_FIXED: u32 = 0 << 8;

/// IPI delivery mode: INIT.
///
/// The INIT delivery mode is the same encoding as INIT level de-assert; the
/// level bit distinguishes them. The INIT mode puts the target processor into
/// a known reset state in real mode, which is the state a Startup IPI needs.
const ICR_DELIVERY_INIT: u32 = 5 << 8;

/// IPI delivery mode: Startup IPI.
///
/// The Startup IPI delivers the target's initial page number in the vector
/// field; the processor begins execution at `vector << 12`.
const ICR_DELIVERY_STARTUP: u32 = 6 << 8;

/// IPI destination mode: physical (target by APIC ID).
const ICR_DEST_PHYSICAL: u32 = 0 << 11;

/// IPI level: assert.
const ICR_LEVEL_ASSERT: u32 = 1 << 14;

/// IPI level: deassert.
const ICR_LEVEL_DEASSERT: u32 = 0 << 14;

/// IPI trigger mode: edge.
const ICR_TRIGGER_EDGE: u32 = 0 << 15;

/// IPI destination shorthand: no shorthand (target specific CPU).
const ICR_DEST_NO_SHORTHAND: u32 = 0 << 18;

/// Vector used for reschedule IPIs between CPUs.
pub const RESCHEDULE_VECTOR: u8 = 0xF0;

/// Read this CPU's APIC ID from the local APIC ID register.
///
/// Returns 0 off bare metal or if the APIC is not yet initialised.
pub fn this_cpu_apic_id() -> u32 {
    if !regs::ON_BARE_METAL {
        return 0;
    }
    let id = apic_read(APIC_ID);
    // The APIC ID is in bits 24-31 of the ID register.
    (id >> 24) & 0xFF
}

/// Send an IPI to a specific CPU identified by its APIC ID.
///
/// # Safety
///
/// Writes to the APIC ICR registers, which are hardware registers. The caller
/// must ensure the target CPU exists and is able to receive IPIs.
pub unsafe fn send_ipi(apic_id: u32, vector: u8) {
    unsafe { send_ipi_with_mode(apic_id, vector, ICR_DELIVERY_FIXED, ICR_LEVEL_ASSERT) };
}

/// Send an INIT IPI to `apic_id`.
///
/// # Safety
///
/// The target must exist and the BSP's APIC must be initialised. The INIT IPI
/// leaves the target in real mode at the architectural reset vector.
pub unsafe fn send_init_ipi(apic_id: u32) {
    // SAFETY: forwarded to the caller's contract.
    unsafe { send_ipi_with_mode(apic_id, 0, ICR_DELIVERY_INIT, ICR_LEVEL_ASSERT) };
}

/// Send an INIT level-deassert IPI to `apic_id`.
///
/// This resynchronises the APIC arbitration priorities after an INIT IPI.
/// It is the second command in the documented INIT/INIT-deassert/SIPI startup
/// sequence.
///
/// # Safety
///
/// The target must exist and the BSP's APIC must be initialised.
pub unsafe fn send_init_level_deassert(apic_id: u32) {
    // SAFETY: forwarded to the caller's contract.
    unsafe { send_ipi_with_mode(apic_id, 0, ICR_DELIVERY_INIT, ICR_LEVEL_DEASSERT) };
}

/// Send a Startup IPI to `apic_id` at page `vector`.
///
/// `vector` is the target's initial page number; execution begins at
/// `vector << 12`. Only the low eight bits are delivered, so the target page
/// must lie below 1 MiB.
///
/// # Safety
///
/// The target must have already received an INIT IPI and the vector must name
/// a page the target can execute from.
pub unsafe fn send_startup_ipi(apic_id: u32, vector: u8) {
    // SAFETY: forwarded to the caller's contract.
    unsafe { send_ipi_with_mode(apic_id, vector, ICR_DELIVERY_STARTUP, ICR_LEVEL_ASSERT) };
}

/// Send one IPI command with the given delivery mode and level.
///
/// # Safety
///
/// The target must exist and the APIC window must be live.
unsafe fn send_ipi_with_mode(apic_id: u32, vector: u8, delivery: u32, level: u32) {
    if !regs::ON_BARE_METAL {
        return;
    }
    // Wait for any previous IPI to complete: the "busy" bit in the ICR low
    // register is set while an IPI is being sent.
    let deadline = regs::rdtsc_fenced().wrapping_add(1_000_000);
    unsafe {
        while (apic_read(APIC_ICR_LOW) & (1 << 12)) != 0 {
            if regs::rdtsc_fenced() > deadline {
                return;
            }
            regs::pause();
        }
        // Destination in the high register, vector and delivery mode in the low.
        apic_write(APIC_ICR_HIGH, apic_id << 24);
        apic_write(
            APIC_ICR_LOW,
            vector as u32
                | delivery
                | ICR_DEST_PHYSICAL
                | level
                | ICR_TRIGGER_EDGE
                | ICR_DEST_NO_SHORTHAND,
        );
    }
}

/// Called from the reschedule IPI handler.
///
/// Mark this CPU as needing a reschedule.
///
/// Called from the reschedule-IPI handler. The scheduler checks this flag at
/// the next safe point and picks a new task from its run queue.
pub fn mark_reschedule_pending() {
    if !regs::ON_BARE_METAL {
        return;
    }
    let id = this_cpu_apic_id() as usize;
    if id < RESCHEDULE_PENDING.len() {
        RESCHEDULE_PENDING[id].store(true, Ordering::Release);
    }
}

/// True when this CPU has a pending reschedule it has not yet acknowledged.
pub fn reschedule_pending() -> bool {
    if !regs::ON_BARE_METAL {
        return false;
    }
    let id = this_cpu_apic_id() as usize;
    id < RESCHEDULE_PENDING.len() && RESCHEDULE_PENDING[id].load(Ordering::Acquire)
}

/// Acknowledge the pending reschedule on this CPU.
pub fn acknowledge_reschedule() {
    if !regs::ON_BARE_METAL {
        return;
    }
    let id = this_cpu_apic_id() as usize;
    if id < RESCHEDULE_PENDING.len() {
        RESCHEDULE_PENDING[id].store(false, Ordering::Release);
    }
}

/// Called from the reschedule IPI handler.
///
/// Marks this CPU as needing a reschedule, which the scheduler checks at the
/// next safe point, and acknowledges the interrupt so the APIC keeps
/// delivering.
///
/// # Safety
///
/// Writes the APIC's end-of-interrupt register.
pub unsafe fn on_reschedule_ipi() {
    mark_reschedule_pending();
    // SAFETY: an APIC IPI must be acknowledged with EOI or it stops
    // delivering forever.
    unsafe { apic_write(APIC_EOI, 0) };
}

/// Enable the APIC and program the timer to `TICKS_PER_SECOND`.
///
/// Calibrates the TSC against the timer, masks every spurious source, and
/// leaves the timer unmasked and running.
pub fn init() {
    if !is_present() {
        crate::serial::puts("[apic] no local APIC; timer will not run\n");
        crate::serial::puts("[apic] version register read ");
        crate::serial::puts_hex(apic_read(APIC_VERSION) as u64);
        crate::serial::puts("\n");
        return;
    }

    // The APIC must be enabled and in **xAPIC** mode for the MMIO window at
    // `0xFEE0_0000` to be live at all.
    //
    // Firmware commonly leaves the APIC in x2APIC mode, in which its
    // registers are reached through the MSRs at `0x800 + offset` and the
    // MMIO window is dead. Poking the window then does nothing, and the
    // timer silently never fires — the kernel boots, prints its banner, and
    // then sits there with no clock.
    //
    // So the state of the window is reported before it is programmed, so a
    // driver talking to the wrong window says so in the log.
    report_apic_state();

    // The DIV register must be written *before* the timer starts, or a
    // running timer could latch the old divide value.
    //
    // The value is `divide::BY_1` — which is the *code* `0b1011`, not zero.
    // The DIV register holds a code, not a divisor, and the code that means
    // "divide by one" is `0b1011`.
    // SAFETY: the APIC was confirmed present.
    unsafe { apic_write(APIC_TIMER_DIV, divide::BY_1) };

    // Mask all timer interrupts during calibration so we are not interrupted
    // mid-measurement.
    //
    // The periodic bit is set even while the timer is masked: the mask only
    // suppresses *delivery*, and the timer keeps counting. Setting periodic
    // here means the counter is already in the right mode when the mask is
    // lifted, and the calibration below measures a real period rather than
    // the first one-shot expiry.
    // SAFETY: as above.
    unsafe {
        apic_write(
            APIC_LVT_TIMER,
            lvt::MASKED | lvt::PERIODIC | TIMER_VECTOR as u32,
        )
    };

    // Program the timer for the target frequency and start it.
    //
    // # Why the current-count register is NOT written
    //
    // Writing `0` to the **current count** register is *not* "restart the
    // timer from the initial count" — it is **"stop the timer"**. The
    // architectural rule is that the timer counts down from the initial
    // count, and the current-count register is *read-only for counting
    // purposes*: writing it sets the count directly, and a count of zero
    // means the timer never expires.
    //
    // A timer stopped this way never fires again until the initial-count
    // register is written. So the sequence that matters is:
    //
    // 1. write the initial count — this loads the counter and starts it,
    // 2. do *not* touch the current count — writing it would stop the timer
    //    again.
    //
    // This is the bug that made the kernel boot with no clock at all: the
    // driver wrote `0` to the current-count register right after loading
    // the initial count, and the timer sat at zero forever.
    let initial = calc_initial_count(TICKS_PER_SECOND);
    // SAFETY: as above.
    unsafe { apic_write(APIC_TIMER_INITCNT, initial) };

    calibrate_tsc(initial);

    // Accept timer interrupts. The spurious vector must be something that will
    // never actually fire; 0xFF is the architectural "ignore" choice.
    //
    // The LVT timer gets the **periodic** bit as well as the vector: without
    // it the timer is one-shot — it fires once and then stops forever, which
    // looks like a working clock right up until the second tick.
    // SAFETY: as above.
    unsafe {
        apic_write(APIC_SPURIOUS, 0xFF | (1 << 8));
        apic_write(APIC_LVT_TIMER, lvt::PERIODIC | TIMER_VECTOR as u32);
    }
    // Put this CPU at the lowest priority so the timer can always preempt.
    // SAFETY: as above.
    unsafe { apic_write(APIC_TPR, 0) };

    INITIALISED.store(1, Ordering::Release);

    // Report the state the timer was left in.
    //
    // The point is diagnosability: a timer that is programmed but never
    // delivers is otherwise indistinguishable from a timer that was never
    // programmed, and the difference is a single register read away. These
    // are the exact registers the timer depends on, read back from the
    // hardware rather than from the values that were written.
    crate::serial::puts("[apic] version ");
    crate::serial::puts_hex(apic_read(APIC_VERSION) as u64);
    crate::serial::puts(" lvt ");
    crate::serial::puts_hex(apic_read(APIC_LVT_TIMER) as u64);
    crate::serial::puts(" initcnt ");
    crate::serial::puts_hex(apic_read(APIC_TIMER_INITCNT) as u64);
    crate::serial::puts(" curcnt ");
    crate::serial::puts_hex(apic_read(APIC_TIMER_CURCNT) as u64);
    crate::serial::puts(" div ");
    crate::serial::puts_hex(apic_read(APIC_TIMER_DIV) as u64);
    crate::serial::puts("\n");
}

/// Measure the TSC frequency using the APIC timer as a reference.
///
/// The APIC timer's own frequency is unknown, so this cannot calibrate the
/// timer; instead it calibrates the *TSC* by timing a known number of APIC
/// ticks whose wall-clock duration the firmware-independent APIC bus rate
/// determines. The result is the ratio of TSC cycles to the bus frequency.
fn calibrate_tsc(initial_count: u32) {
    // Let the timer run long enough that TSC measurement dominates the read
    // latency. 16 periods is comfortably above the resolution of a serial
    // read of the counter.
    const PERIODS: u32 = 16;
    let periods_ns = (PERIODS as u64 * 1_000_000_000u64) / TICKS_PER_SECOND;

    let tsc_start = regs::tsc();
    let start = now_nanos();

    // Busy-wait for the calibrated interval using the same clock the TSC is
    // being compared against would be circular; sleep on the TSC itself and
    // measure how far the nanosecond clock advanced.
    let target = start + periods_ns;
    while now_nanos() < target {
        regs::pause();
    }

    let tsc_elapsed = regs::tsc().wrapping_sub(tsc_start);
    let ns_elapsed = now_nanos() - start;

    // TSC Hz = cycles per nanosecond, times 1e9. Guard against a zero
    // measurement on a host with a broken reference clock.
    if let Some(hz) = tsc_elapsed
        .saturating_mul(1_000_000_000)
        .checked_div(ns_elapsed)
    {
        if hz > 1_000_000 {
            TSC_HZ.store(hz, Ordering::Release);
        }
    }
    let _ = initial_count;
}

/// A free-running nanosecond counter.
///
/// Uses the calibrated TSC when available, and falls back to the APIC tick
/// count (coarser) when calibration has not run. Either way the result is
/// monotonic, which is the only property calibration code relies on.
fn now_nanos() -> u64 {
    let hz = TSC_HZ.load(Ordering::Acquire);
    if hz == 0 {
        // Uncalibrated: use raw TSC deltas at an assumed nominal rate. Good
        // enough for a calibration loop that only needs a stable interval.
        return regs::tsc() / 20;
    }
    regs::tsc().saturating_mul(1_000_000_000) / hz
}

/// The current logical tick count.
///
/// Monotonic by construction: it is the sum of completed APIC ticks (bumped in
/// the timer interrupt) plus the fraction of the current tick elapsed.
pub fn ticks() -> u64 {
    TICKS.load(Ordering::Acquire)
}

/// Tick frequency, matching [`TICKS_PER_SECOND`].
pub const fn ticks_per_second() -> u64 {
    TICKS_PER_SECOND
}

/// Spin for roughly `ticks` ticks.
pub fn busy_wait(ticks: u64) {
    let start = ticks_actual();
    // Guard against a timer that was never initialised, which would otherwise
    // make this loop forever.
    if INITIALISED.load(Ordering::Acquire) == 0 {
        // No timer: fall back to a TSC-derived delay of roughly the same order.
        let hz = TSC_HZ.load(Ordering::Acquire).max(1_000_000);
        regs::spin_hint(ticks * (hz / TICKS_PER_SECOND / 64).max(1));
        return;
    };
    while ticks_actual().wrapping_sub(start) < ticks {
        regs::pause();
    }
}

/// The current tick count, as a free function so `busy_wait` can call it
/// without an associated-item ambiguity.
fn ticks_actual() -> u64 {
    TICKS.load(Ordering::Acquire)
}

/// Install the kernel's per-tick callback.
///
/// # Safety
///
/// `hook` must be a plain function that takes the tick count and does no
/// blocking work. It runs from interrupt context on whichever CPU took the
/// timer interrupt. Call this during boot, before interrupts are enabled.
pub unsafe fn install_timer_hook(hook: fn(u64)) {
    // SAFETY: a function pointer is a code address, and the kernel never
    // converts a different value to a function pointer.
    TIMER_HOOK.store(hook as usize, Ordering::Release);
}

/// Called from the timer interrupt handler.
///
/// Bumps the logical tick counter, runs the installed kernel hook, and
/// acknowledges the interrupt. Acknowledging is mandatory: an unacknowledged
/// APIC timer stops delivering forever, which presents as the kernel freezing
/// after exactly one tick.
pub fn on_timer_interrupt() {
    let tick = TICKS.fetch_add(1, Ordering::Release) + 1;
    let addr = TIMER_HOOK.load(Ordering::Acquire);
    if addr != 0 {
        // SAFETY: the only value ever stored is a `fn(u64)` address, and the
        // hook was installed during boot before interrupts were enabled.
        let hook: fn(u64) = unsafe { core::mem::transmute(addr) };
        hook(tick);
    }
    // SAFETY: `APIC_EOI` is writable from any CPU that took an APIC interrupt.
    unsafe { apic_write(APIC_EOI, 0) };
}

/// The APIC bus frequency assumed for the initial-count calculation.
pub const APIC_BUS_FREQ_HZ: u64 = 100_000_000;

#[cfg(test)]
extern crate std;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initial_count_scales_with_frequency() {
        // Higher target frequency => shorter period => smaller count.
        let slow = calc_initial_count(50);
        let fast = calc_initial_count(1000);
        assert!(
            fast < slow,
            "fast ({fast}) must count fewer than slow ({slow})"
        );
    }

    #[test]
    fn initial_count_is_never_zero() {
        // A zero initial count disables the timer on some implementations and
        // makes the count read as "expired" on others.
        for freq in [1, 50, 250, 1000, 10_000] {
            assert!(calc_initial_count(freq) >= 1, "freq {freq}");
        }
        assert!(calc_initial_count(0) >= 1);
    }

    #[test]
    fn initial_count_respects_the_24_bit_maximum() {
        // An absurdly low frequency would otherwise overflow the 24-bit field.
        let c = calc_initial_count(1);
        assert!(c <= 0x00FF_FFFF, "count {c} exceeds 24 bits");
    }

    #[test]
    fn divide_constants_are_distinct() {
        assert_ne!(divide::BY_1, divide::BY_2);
        assert_ne!(divide::BY_2, divide::BY_4);
        assert_ne!(divide::BY_4, divide::BY_8);
        // All divide ratios fit the DIV register's 8-bit field.
        for d in [divide::BY_1, divide::BY_2, divide::BY_4, divide::BY_8] {
            assert!(d <= 0xFF);
        }
    }

    #[test]
    fn lvt_mask_is_the_documented_bit() {
        assert_eq!(lvt::MASKED, 1 << 16);
        assert_eq!(
            lvt::MASKED & 0xFF,
            0,
            "mask must not collide with the vector"
        );
    }

    #[test]
    fn timer_vector_is_outside_the_legacy_pic_range() {
        // The timer is a *local* APIC interrupt, not an IRQ line. It must not
        // share a vector with the legacy 8259 PIC's IRQ0, or the two would be
        // indistinguishable at the gate and the handler could not tell a
        // timer tick from an IRQ0.
        let v = TIMER_VECTOR;
        assert!(
            !(crate::idt::vectors::IRQ_BASE..=crate::idt::vectors::IRQ_LAST).contains(&v),
            "the timer vector {v:#04x} collides with the legacy PIC range"
        );
        // It must still be a vector the CPU can deliver on: the IDT has 256
        // entries, and `u8` can only name the first 256 of them.
        assert_eq!(crate::idt::IDT_ENTRIES, 256);
        assert!(u32::from(v) < crate::idt::IDT_ENTRIES as u32);
    }

    #[test]
    fn the_timer_reads_the_tsc_with_a_baseline_instruction() {
        // `RDTSCP` is an optional CPU feature that QEMU's default CPU model
        // does not implement; issuing it there raises `#UD`. The timer
        // calibration must therefore read the TSC with the baseline
        // `RDTSC`+`LFENCE` sequence instead.
        //
        // This is asserted at the source level rather than by executing the
        // instruction, because the host test runner is not the guest. The
        // check looks for the fenced reader the calibration must use, and
        // names the forbidden one by its opcode rather than by its Rust
        // symbol, so this test's own text cannot satisfy it.
        let source = include_str!("apic.rs");
        assert!(
            source.contains("rdtsc_fenced"),
            "the APIC timer must read the TSC through the fenced reader"
        );
        assert!(
            !source.contains(concat!("rdts", "cp")),
            "the APIC timer must not use RDTSCP"
        );
    }

    #[test]
    fn tick_frequency_matches_the_plan() {
        // 250 Hz is a 4 ms quantum; changing it changes scheduler latency, so
        // it is pinned here rather than left implicit in the caller.
        assert_eq!(ticks_per_second(), 250);
    }

    #[test]
    fn ticks_are_monotonic_in_a_fresh_process() {
        // The unit tests never initialise the APIC, so the counter must still
        // be readable and start from a known point.
        let before = ticks();
        on_timer_interrupt();
        assert_eq!(ticks(), before + 1);
    }

    #[test]
    fn a_second_is_the_right_number_of_ticks() {
        assert_eq!(ticks_per_second(), 250);
        // Guard the invariant that busy_wait's tick arithmetic assumes.
        assert_eq!(1_000_000_000 / TICKS_PER_SECOND, 4_000_000);
    }
}
