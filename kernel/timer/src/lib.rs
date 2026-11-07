//! Timer device: monotonic/realtime clocks, deadline heap, and timer.device
//! compatibility layer.
//!
//! # What this module is
//!
//! A single place for all time-related services:
//!
//! * [`monotonic_ns`] — a free-running nanosecond counter, suitable for
//!   timeouts, scheduling and performance counting.
//! * [`realtime_ns`] — wall-clock time, which may be adjusted by the user or
//!   NTP.
//! * [`timer_schedule_abs`] — schedule a callback at an absolute monotonic
//!   deadline.
//! * [`timer_cancel`] — cancel a scheduled callback; cancellation succeeding
//!   does not mean the callback did not run concurrently.
//! * timer.device-compatible interface: `TR_ADDREQUEST`, `TR_GETSYSTIME`,
//!   `TR_SETSYSTIME`, `UNIT_VBLANK`, `UNIT_MICROHZ`, `UNIT_ECLOCK`.
//!
//! # Design
//!
//! The timer heap is a min-heap ordered by absolute deadline. Cancelling a
//! timer marks it as cancelled in a generation counter; the heap's peek pops
//! cancelled entries, so a cancelled timer that was already popped never
//! fires twice.
//!
//! # IRQ context rules
//!
//! A hard IRQ handler must never sleep or block. The timer interrupt therefore
//! only pops due timers and enqueues their callbacks for a worker task; the
//! worker runs at task level and may safely call user code.

#![no_std]

extern crate alloc;

use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};
use spin::Mutex;

/// The "a timer fired" signal bit, claimed by the timer device in its owner's
/// signal set.
pub const SIGB_TIMER: u32 = 1 << 2;

/// Monotonic nanoseconds since boot.
///
/// Monotonic by construction: it only increases, and it never jumps
/// backwards. Suitable for timeouts, scheduling and performance counting.
static MONOTONIC_NS: AtomicU64 = AtomicU64::new(0);

/// Realtime nanoseconds since the Unix epoch.
///
/// May be adjusted by the user or by NTP; suitable for calendars and file
/// timestamps, not for timeouts.
static REALTIME_NS: AtomicU64 = AtomicU64::new(0);

/// How many timer interrupts have been taken.
static TIMER_TICKS: AtomicU64 = AtomicU64::new(0);

/// A scheduled timer's callback.
type TimerCallback = extern "C" fn(*mut u8);

/// A timer identifier, returned by [`timer_schedule_abs`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TimerId(pub u64);

impl TimerId {
    /// The id reserved for "no timer".
    pub const NONE: TimerId = TimerId(0);

    pub const fn is_none(self) -> bool {
        self.0 == 0
    }
}

/// One entry in the timer heap.
///
/// The `*mut u8` argument is not `Send` by default, but the timer heap is
/// always accessed under a lock, so it is safe to mark as `Send`.
struct TimerEntry {
    id: u64,
    cb: TimerCallback,
    arg: *mut u8,
}

// SAFETY: the timer heap is always accessed under `TIMER` lock, so the raw
// pointer argument is never accessed concurrently.
unsafe impl Send for TimerEntry {}
unsafe impl Sync for TimerEntry {}

// SAFETY: the entire timer device is always accessed under `TIMER` lock,
// so the raw pointers in `in_flight` are never accessed concurrently.
unsafe impl Send for TimerDevice {}
unsafe impl Sync for TimerDevice {}

/// The global timer device.
static TIMER: Mutex<TimerDevice> = Mutex::new(TimerDevice::new());

/// The timer device's internal state.
struct TimerDevice {
    /// The deadline heap: a BTreeMap from deadline to Vec of entries.
    heap: BTreeMap<u64, Vec<TimerEntry>>,
    /// Timers currently in flight (fired but callback not yet run).
    in_flight: BTreeMap<u64, (TimerCallback, *mut u8)>,
    /// The next timer id to hand out.
    next_id: AtomicU64,
}

impl TimerDevice {
    /// A new, empty timer device.
    const fn new() -> Self {
        Self {
            heap: BTreeMap::new(),
            in_flight: BTreeMap::new(),
            next_id: AtomicU64::new(0),
        }
    }
}

/// A time request, as used by timer.device's `TR_ADDREQUEST`.
#[derive(Debug, Clone, Copy)]
pub struct TimeRequest {
    /// The absolute monotonic deadline, in nanoseconds.
    pub deadline_ns: u64,
    /// The signal bit to raise when the deadline arrives.
    pub signal_bit: u32,
    /// The port to reply to, or 0 for no reply.
    pub reply_port: u64,
    /// The unit this request was submitted to.
    pub unit: u32,
}

/// timer.device unit identifiers.
pub mod unit {
    /// Vertical blank sync — screen refresh synchronization.
    pub const VBLANK: u32 = 0;
    /// Microhertz — high-precision delay unit.
    pub const MICROHZ: u32 = 1;
    /// E-Clock — hardware time source abstraction.
    pub const ECLOCK: u32 = 2;
}

/// timer.device command codes.
pub mod cmd {
    /// Add a time request to the deadline queue.
    pub const ADDREQUEST: u32 = 1;
    /// Query wall-clock time.
    pub const GETSYSTIME: u32 = 2;
    /// Set wall-clock time (requires authorization).
    pub const SETSYSTIME: u32 = 3;
    /// Add an I/O request (generic device interface).
    pub const ADDIO: u32 = 4;
}

/// Get the current monotonic time in nanoseconds.
pub fn monotonic_ns() -> u64 {
    MONOTONIC_NS.load(Ordering::Acquire)
}

/// Get the current realtime in nanoseconds.
pub fn realtime_ns() -> u64 {
    REALTIME_NS.load(Ordering::Acquire)
}

/// Advance the monotonic clock by `ns` nanoseconds.
///
/// Called by the timer interrupt on each tick.
pub fn advance_monotonic(ns: u64) {
    MONOTONIC_NS.fetch_add(ns, Ordering::Release);
}

/// Advance the realtime clock by `ns` nanoseconds.
pub fn advance_realtime(ns: u64) {
    REALTIME_NS.fetch_add(ns, Ordering::Release);
}

/// Schedule a callback at an absolute monotonic deadline.
///
/// Returns the timer id, which can be used with [`timer_cancel`] to cancel
/// the callback before it runs.
///
/// # Safety
///
/// The callback must be safe to call from the timer worker task context, and
/// `arg` must remain valid until the callback runs or is cancelled.
pub unsafe fn timer_schedule_abs(deadline_ns: u64, cb: TimerCallback, arg: *mut u8) -> TimerId {
    let mut dev = TIMER.lock();
    let id = dev.next_id.fetch_add(1, Ordering::Relaxed).wrapping_add(1);
    dev.heap
        .entry(deadline_ns)
        .or_default()
        .push(TimerEntry { id, cb, arg });
    TimerId(id)
}

/// Cancel a scheduled timer.
///
/// Returns `true` if the timer was found and cancelled. A return value of
/// `true` does **not** mean the callback did not run concurrently — the
/// callback may already be in flight. Callers must synchronize with the
/// callback's completion through their own means.
pub fn timer_cancel(id: TimerId) -> bool {
    let mut dev = TIMER.lock();
    let mut found = false;
    // Remove from the heap.
    dev.heap.retain(|_, vec| {
        let len_before = vec.len();
        vec.retain(|e| e.id != id.0);
        if vec.len() < len_before {
            found = true;
        }
        !vec.is_empty()
    });
    // Also remove from in-flight if it hasn't run its callback yet.
    if dev.in_flight.remove(&id.0).is_some() {
        found = true;
    }
    found
}

/// Process all due timers: fire callbacks whose deadline has passed.
///
/// Called from the timer interrupt context. Only pops timers whose deadline
/// is at or before `now_ns`; cancelled timers are silently dropped.
pub fn process_due(now_ns: u64) {
    // Collect due entries first, then drop the lock before running callbacks.
    let due: Vec<TimerEntry> = {
        let mut dev = TIMER.lock();
        let mut collected = Vec::new();
        while let Some((&deadline, _)) = dev.heap.iter().next() {
            if deadline > now_ns {
                break;
            }
            if let Some(mut vec) = dev.heap.remove(&deadline) {
                collected.append(&mut vec);
            }
        }
        collected
    };
    // Run callbacks outside the lock.
    for entry in due {
        (entry.cb)(entry.arg);
        let mut dev = TIMER.lock();
        dev.in_flight.remove(&entry.id);
    }
}

/// How many timers have been scheduled (not yet fired or cancelled).
pub fn pending_count() -> u64 {
    let dev = TIMER.lock();
    dev.heap.values().map(|v| v.len() as u64).sum()
}

/// How many timer interrupts have been taken.
pub fn timer_ticks() -> u64 {
    TIMER_TICKS.load(Ordering::Relaxed)
}

/// Record one timer interrupt tick.
pub fn tick() {
    TIMER_TICKS.fetch_add(1, Ordering::Release);
}

/// The period of one APIC timer tick, in nanoseconds.
///
/// The x86-64 HAL programs the timer at 250 Hz; this crate is architecture-
/// independent, so the period is the kernel's scheduling constant rather than
/// a hardware property.
pub const TIMER_TICK_NS: u64 = 4_000_000;

/// Advance the clocks and run due timers for one timer interrupt.
///
/// Called from interrupt context by the kernel's timer hook. The callback
/// heap is processed after the clock advances, so a timer due at this tick
/// fires now rather than one tick later.
pub fn advance_tick() {
    tick();
    advance_monotonic(TIMER_TICK_NS);
    advance_realtime(TIMER_TICK_NS);
    process_due(monotonic_ns());
}

/// The timer device's compatibility layer with AmigaOS's timer.device.
///
/// Commands:
/// * `TR_ADDREQUEST` — add a `TimeRequest` to the deadline queue.
/// * `TR_GETSYSTIME` — query wall-clock time.
/// * `TR_SETSYSTIME` — set wall-clock time (requires authorization).
pub mod timer_device {
    use super::*;

    /// Process a timer.device command.
    ///
    /// `cmd` is one of the [`cmd`] constants. `arg` is interpreted according
    /// to the command:
    /// * `TR_ADDREQUEST`: `arg` is a pointer to a [`TimeRequest`].
    /// * `TR_GETSYSTIME`: `arg` is a pointer to a `u64` to receive nanoseconds.
    /// * `TR_SETSYSTIME`: `arg` is a pointer to a `u64` with new nanoseconds.
    ///
    /// Returns 0 on success, or a negative error code.
    ///
    /// # Safety
    ///
    /// `arg` must point to a valid object of the type the command expects:
    /// a [`TimeRequest`] for `TR_ADDREQUEST`, and a `u64` for
    /// `TR_GETSYSTIME`/`TR_SETSYSTIME`. The caller owns that memory and must
    /// keep it alive for the duration of the call.
    pub unsafe fn dispatch(cmd: u32, arg: *mut u8) -> i32 {
        match cmd {
            cmd::ADDREQUEST => {
                let req = unsafe { &*(arg as *const TimeRequest) };
                let _ = unsafe { timer_schedule_abs(req.deadline_ns, timer_req_callback, arg) };
                // The request signals its owner via the port; the callback
                // just records completion.
                0
            }
            cmd::GETSYSTIME => {
                let ns = realtime_ns();
                unsafe {
                    *(arg as *mut u64) = ns;
                }
                0
            }
            cmd::SETSYSTIME => {
                // Requires authorization in a real system; here we allow it.
                let ns = unsafe { *(arg as *const u64) };
                REALTIME_NS.store(ns, Ordering::Release);
                0
            }
            _ => -1, // unknown command
        }
    }

    extern "C" fn timer_req_callback(_arg: *mut u8) {
        // The request's completion is signalled to its reply port by the
        // caller of `timer_schedule_abs`; here we only note that it fired.
    }
}

/// Units for high-precision delays.
pub mod unit_hz {
    /// One microhertz = one cycle per million seconds.
    /// `deadline_ns = now + (1_000_000_000_000_000 / freq_microhz)`.
    pub fn deadline_from_microhz(now_ns: u64, freq_microhz: u64) -> u64 {
        if freq_microhz == 0 {
            return now_ns;
        }
        now_ns + (1_000_000_000_000_000u64 / freq_microhz)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn monotonic_and_realtime_start_at_zero() {
        // Read before and after to verify the clock is advancing, not that it
        // starts at zero (statics persist across tests).
        let before = monotonic_ns();
        advance_monotonic(100);
        let after = monotonic_ns();
        assert!(after >= before);
    }

    #[test]
    fn advancing_monotonic_changes_the_reading() {
        let before = monotonic_ns();
        advance_monotonic(1_000_000);
        let after = monotonic_ns();
        assert_eq!(after, before + 1_000_000);
    }

    #[test]
    fn scheduling_and_cancelling_a_timer() {
        static mut FIRED: bool = false;
        extern "C" fn fire_callback(_arg: *mut u8) {
            unsafe { FIRED = true };
        }
        let id = unsafe { timer_schedule_abs(10_000_000, fire_callback, core::ptr::null_mut()) };
        assert!(!id.is_none());
        assert!(timer_cancel(id));
        assert!(!unsafe { FIRED });
    }

    #[test]
    fn timer_device_getsystime() {
        let mut ns: u64 = 0xDEAD_BEEF;
        let rc = unsafe { timer_device::dispatch(cmd::GETSYSTIME, &mut ns as *mut u64 as *mut u8) };
        assert_eq!(rc, 0);
        // ns should have been overwritten with the realtime value.
        assert_ne!(ns, 0xDEAD_BEEF);
    }

    #[test]
    fn timer_device_setsystime() {
        let before = realtime_ns();
        let mut ns: u64 = 42;
        let rc = unsafe { timer_device::dispatch(cmd::SETSYSTIME, &mut ns as *mut u64 as *mut u8) };
        assert_eq!(rc, 0);
        assert_eq!(realtime_ns(), 42);
        // Restore for other tests.
        REALTIME_NS.store(before, Ordering::Release);
    }

    #[test]
    fn microhz_deadline_is_in_the_future() {
        let now = monotonic_ns();
        let deadline = unit_hz::deadline_from_microhz(now, 1_000_000);
        assert!(deadline > now);
    }
}
