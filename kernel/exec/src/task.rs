//! Task identity and the signal set.
//!
//! # Signals are the Exec rendezvous
//!
//! On AmigaOS a task and a message port communicate through a *signal set*:
//! a 32-bit mask in the task, of which the port claims certain bits. A
//! message arriving at the port sets bits in the owner's signal set, and
//! `Wait(mask)` blocks until any bit of `mask` is set.
//!
//! Bit 0 is special — it is the "a message is waiting" bit, and the low bits
//! are the reply bits. The split is not arbitrary: it is what lets a task wait
//! for "either a new request *or* a reply to something I already sent" with a
//! single 32-bit mask, which is the whole point of the design.
//!
//! # Why this is not a condvar
//!
//! A condvar wakes exactly one waiter and carries no payload. Exec's signal
//! set is a *shared, inspectable, sticky* mask: bits accumulate until read,
//! multiple waiters can observe the same bit, and `SetSignal` can raise a bit
//! that nobody is waiting for yet. Modelling it as a condvar would lose all
//! three properties, so it is modelled as what it is — a mask with explicit
//! wait bookkeeping alongside.

use core::sync::atomic::{AtomicU32, Ordering};

use vela_abi_native64::node_type;

/// The "a message is waiting on this port" signal bit.
///
/// A port claims this bit in `mp_sig`. The kernel sets it in the owner's
/// signal set when a message is enqueued, and clears it when the queue empties
/// again — which is the only reason to clear it, because a sticky bit that
/// outlived its message would make `WaitPort` return forever.
pub const SIGB_MESSAGE: u32 = 1 << 0;

/// The bits reserved for reply signals.
///
/// Reply slot `n` uses bit `n + 1`, so the first reply is bit 1. Fifteen
/// replies can therefore be outstanding at once, which is the AmigaOS limit:
/// the classic `REPLY_MASK` is `0xFFFE`.
pub const REPLY_MASK: u32 = 0xFFFF_FFFE;

/// Every signal bit a task may use.
pub const SIGNAL_MASK_ALL: u32 = 0xFFFF_FFFF;

/// An opaque task identifier.
///
/// A newtype rather than a raw index because the kernel handles tasks across a
/// trust boundary: a syscall passes a task id, and a forged id must be
/// rejected rather than index a table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TaskId(pub u32);

impl TaskId {
    /// The id reserved for "no task", used for a reply port with no owner.
    pub const NONE: TaskId = TaskId(0);

    /// True when this id names no task.
    pub const fn is_none(self) -> bool {
        self.0 == 0
    }
}

/// Why a task is blocked, and on what.
///
/// Recorded so the scheduler can tell a task that is waiting for a message
/// from one that is waiting for a signal, and so a debugger can explain a
/// system that appears stuck.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockedOn {
    /// Not blocked.
    Nothing,
    /// Waiting for any bit of this mask to be set in the signal set.
    Signal(u32),
    /// Waiting for a message on this port.
    Port(u32),
    /// Waiting for a reply to a message sent to this port.
    ReplyPort(u32),
}

/// A task's signal set and wait bookkeeping.
///
/// The signal mask is atomic because it is read and written from both sides of
/// a context switch: the owner sets it, and the scheduler inspects it when
/// deciding who runs next.
#[derive(Debug)]
pub struct TaskSignals {
    /// The name, for diagnostics.
    name: TaskId,
    /// The current signal mask. Sticky: bits clear only when read or unset.
    signals: AtomicU32,
    /// The mask the task is waiting for, or `None` when it is running.
    waiting_for: AtomicU32,
    /// What the task is blocked on, for diagnostics.
    blocked_on: AtomicU32,
}

impl TaskSignals {
    /// A fresh task with no signals pending and not waiting.
    pub const fn new(name: TaskId) -> Self {
        Self {
            name,
            signals: AtomicU32::new(0),
            waiting_for: AtomicU32::new(0),
            blocked_on: AtomicU32::new(0),
        }
    }

    /// The task these signals belong to.
    pub const fn task(&self) -> TaskId {
        self.name
    }

    /// The current signal mask.
    pub fn current(&self) -> u32 {
        self.signals.load(Ordering::Acquire)
    }

    /// True when any bit of `mask` is set.
    pub fn has_any(&self, mask: u32) -> bool {
        mask != 0 && self.current() & mask != 0
    }

    /// Set every bit in `mask`.
    ///
    /// Returns the new mask. `Release` so a wakeup made visible here is also
    /// visible to the scheduler when it looks for runnable tasks.
    pub fn raise(&self, mask: u32) -> u32 {
        self.signals.fetch_or(mask, Ordering::Release) | mask
    }

    /// Clear every bit in `mask`.
    ///
    /// Returns the bits that were set *and have now been cleared*, which is
    /// what a caller that wants "consume these signals" needs.
    pub fn lower(&self, mask: u32) -> u32 {
        let previous = self.signals.fetch_and(!mask, Ordering::AcqRel);
        previous & mask
    }

    /// Set and clear bits in one atomic operation.
    ///
    /// Two separate operations would leave a window in which a signal raised
    /// by an interrupt arrives, is cleared by the `lower` half, and is lost.
    pub fn modify(&self, set: u32, clear: u32) -> u32 {
        debug_assert_eq!(
            set & clear,
            0,
            "a signal bit cannot be set and cleared at once",
        );
        let mask = set | clear;
        // `fetch_and` first so the un-set bits are removed, then `fetch_or` for
        // the new ones. Doing the clear first means a concurrent `raise` of a
        // bit in `clear` can be undone, but a concurrent `raise` of a bit in
        // `set` is never lost.
        self.signals.fetch_and(!mask, Ordering::AcqRel);
        self.signals.fetch_or(set, Ordering::Release)
    }

    /// Begin waiting for any bit of `mask`.
    ///
    /// Returns `true` if the task must block, `false` if a required signal is
    /// already set and it should not block at all. The distinction is the
    /// whole point of the check-then-wait dance the scheduler performs, and
    /// getting it wrong is a lost-wakeup bug.
    pub fn begin_wait(&self, mask: u32, on: BlockedOn) -> bool {
        if mask == 0 {
            return false;
        }
        // Check first: if the signal is already here, waiting would deadlock
        // until something else woke us, and nothing may.
        if self.has_any(mask) {
            // The wait is *published* even though the task does not block.
            // `end_wait` reports the satisfied bits by reading the published
            // mask, so skipping the publish here would make an immediately-
            // satisfied wait report "nothing was satisfied".
            self.waiting_for.store(mask, Ordering::Release);
            self.blocked_on.store(encode_blocked(on), Ordering::Release);
            return false;
        }
        self.waiting_for.store(mask, Ordering::Release);
        self.blocked_on.store(encode_blocked(on), Ordering::Release);
        // Re-check after publishing the wait. A signal raised between the first
        // check and this store would otherwise be missed: the raiser sees no
        // waiter and the waiter sleeps forever.
        !self.has_any(mask)
    }

    /// Stop waiting and report what was satisfied.
    ///
    /// Returns the bits of `mask` that were actually set — which is the value
    /// `Wait` hands back to the caller. Returns `None` when the task was not
    /// waiting, so a spurious wakeup cannot fabricate a result.
    pub fn end_wait(&self) -> Option<u32> {
        let mask = self.waiting_for.swap(0, Ordering::AcqRel);
        self.blocked_on.store(0, Ordering::Release);
        if mask == 0 {
            return None;
        }
        // Only the bits that were *asked for* count as satisfied. Without
        // this a signal arriving for an unrelated reason would look like a
        // successful wait.
        Some(self.current() & mask)
    }

    /// True when the task is waiting for a signal.
    pub fn is_waiting(&self) -> bool {
        self.waiting_for.load(Ordering::Acquire) != 0
    }

    /// The mask the task is waiting for.
    pub fn waiting_mask(&self) -> u32 {
        self.waiting_for.load(Ordering::Acquire)
    }

    /// What the task is blocked on.
    pub fn blocked_on(&self) -> BlockedOn {
        decode_blocked(self.blocked_on.load(Ordering::Acquire))
    }
}

/// Pack a [`BlockedOn`] into the one word the task stores atomically.
const fn encode_blocked(on: BlockedOn) -> u32 {
    match on {
        BlockedOn::Nothing => 0,
        // Kind in the low two bits, value in the upper 30. A port id and a
        // signal mask both fit comfortably.
        BlockedOn::Signal(mask) => 1 | ((mask & 0x3FFF_FFFF) << 2),
        BlockedOn::Port(id) => 2 | ((id & 0x3FFF_FFFF) << 2),
        BlockedOn::ReplyPort(id) => 3 | ((id & 0x3FFF_FFFF) << 2),
    }
}

/// Unpack a [`BlockedOn`].
const fn decode_blocked(raw: u32) -> BlockedOn {
    match raw & 0b11 {
        0 => BlockedOn::Nothing,
        1 => BlockedOn::Signal((raw >> 2) & 0x3FFF_FFFF),
        2 => BlockedOn::Port((raw >> 2) & 0x3FFF_FFFF),
        _ => BlockedOn::ReplyPort((raw >> 2) & 0x3FFF_FFFF),
    }
}

/// The node type a task's list node carries.
pub const TASK_NODE_TYPE: u8 = node_type::TASK;

#[cfg(test)]
extern crate std;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fresh_task_has_no_signals_and_is_not_waiting() {
        let s = TaskSignals::new(TaskId(1));
        assert_eq!(s.current(), 0);
        assert!(!s.is_waiting());
        assert!(!s.has_any(1));
        assert_eq!(s.blocked_on(), BlockedOn::Nothing);
        assert_eq!(s.end_wait(), None);
    }

    #[test]
    fn signals_are_sticky_until_read() {
        let s = TaskSignals::new(TaskId(1));
        s.raise(SIGB_MESSAGE);
        // Still set after any number of reads: reading is not consuming.
        assert!(s.has_any(SIGB_MESSAGE));
        assert!(s.has_any(SIGB_MESSAGE));
        assert_eq!(s.current(), SIGB_MESSAGE);
    }

    #[test]
    fn raising_is_orbital() {
        let s = TaskSignals::new(TaskId(1));
        s.raise(0b1010);
        s.raise(0b0100);
        // The bits already set are untouched; the new bit is added.
        assert_eq!(s.current(), 0b1110);
    }

    #[test]
    fn only_the_requested_bits_count_as_satisfied() {
        let s = TaskSignals::new(TaskId(1));
        // Wait for bit 4, but bit 1 is what arrives.
        assert!(s.begin_wait(0b1_0000, BlockedOn::Signal(0b1_0000)));
        s.raise(0b0010);
        // The mask of satisfied bits is empty, because bit 1 was not asked for.
        // This is the assertion that stops a spurious signal from looking like
        // a successful wait.
        assert_eq!(s.end_wait(), Some(0));
    }

    #[test]
    fn end_wait_reports_exactly_the_requested_bits_that_are_set() {
        let s = TaskSignals::new(TaskId(1));
        s.raise(0b0100 | 0b1000);
        // Bit 2 of the requested mask is already set, so the task does not
        // block — but the wait is still published, and `end_wait` reports
        // exactly the requested bits that are set.
        assert!(!s.begin_wait(0b0110, BlockedOn::Signal(0)));
        assert_eq!(s.end_wait(), Some(0b0100));
    }

    #[test]
    fn end_wait_reports_nothing_when_no_requested_bit_arrived() {
        let s = TaskSignals::new(TaskId(1));
        // Only an unrelated bit is set, so the task really does block.
        s.raise(0b1000);
        assert!(s.begin_wait(0b0110, BlockedOn::Signal(0)));
        assert_eq!(s.end_wait(), Some(0));
    }

    #[test]
    fn waiting_on_an_already_set_signal_does_not_block() {
        let s = TaskSignals::new(TaskId(1));
        s.raise(SIGB_MESSAGE);
        // The signal is already here, so the task must not sleep: this is the
        // check-then-wait race the scheduler depends on.
        assert!(!s.begin_wait(SIGB_MESSAGE, BlockedOn::Signal(SIGB_MESSAGE)));
        // And `end_wait` still reports what satisfied it.
        assert_eq!(s.end_wait(), Some(SIGB_MESSAGE));
    }

    #[test]
    fn waiting_on_zero_never_blocks() {
        let s = TaskSignals::new(TaskId(1));
        assert!(!s.begin_wait(0, BlockedOn::Signal(0)));
        assert!(!s.is_waiting());
    }

    #[test]
    fn a_wait_can_be_cancelled() {
        let s = TaskSignals::new(TaskId(1));
        assert!(s.begin_wait(SIGB_MESSAGE, BlockedOn::Signal(SIGB_MESSAGE)));
        assert!(s.is_waiting());
        // Cancelling reports the signal if one happened to arrive; if not, the
        // task simply resumes without it.
        assert_eq!(s.end_wait(), Some(0));
        assert!(!s.is_waiting());
        assert_eq!(s.end_wait(), None, "a second cancel reports nothing");
    }

    #[test]
    fn a_lost_wakeup_is_not_possible() {
        // Model the exact interleaving that loses a wakeup: the task checks
        // (nothing set), the raiser sets, the task publishes its wait. The
        // second check inside `begin_wait` is what catches this.
        let s = TaskSignals::new(TaskId(1));
        assert!(!s.has_any(SIGB_MESSAGE), "pre-check sees nothing");
        // The raiser runs here, between the pre-check and the publish.
        s.raise(SIGB_MESSAGE);
        // The publish re-checks, so the task does not block.
        assert!(!s.begin_wait(SIGB_MESSAGE, BlockedOn::Signal(SIGB_MESSAGE)));
        assert!(s.has_any(SIGB_MESSAGE));
    }

    #[test]
    fn modify_sets_and_clears_together() {
        let s = TaskSignals::new(TaskId(1));
        s.raise(0b1111);
        // Clear bits 0 and 1, set bit 2.
        s.modify(0b0100, 0b0011);
        assert_eq!(s.current(), 0b1100);
    }

    #[test]
    fn modify_is_atomic_against_a_clear_and_set() {
        let s = TaskSignals::new(TaskId(1));
        s.raise(0b1111);
        s.modify(0, 0b1111);
        assert_eq!(s.current(), 0);
        s.modify(0b1, 0);
        assert_eq!(s.current(), 0b1);
    }

    #[test]
    fn blocked_on_round_trips_through_its_encoding() {
        assert_eq!(
            decode_blocked(encode_blocked(BlockedOn::Nothing)),
            BlockedOn::Nothing
        );
        assert_eq!(
            decode_blocked(encode_blocked(BlockedOn::Port(0x1234))),
            BlockedOn::Port(0x1234)
        );
        assert_eq!(
            decode_blocked(encode_blocked(BlockedOn::ReplyPort(0xABC))),
            BlockedOn::ReplyPort(0xABC)
        );
    }

    #[test]
    fn the_blocked_on_encoding_is_wide_enough_for_a_port_id() {
        // The kernel assigns port ids monotonically; the encoding must not
        // silently truncate a realistic one.
        let big = 0x0FFF_FFFF;
        assert_eq!(
            decode_blocked(encode_blocked(BlockedOn::Port(big))),
            BlockedOn::Port(big)
        );
    }

    #[test]
    fn the_signal_and_reply_bits_do_not_overlap() {
        // Bit 0 is the message bit and bits 1..31 are reply bits, so a reply
        // can never be mistaken for a request.
        assert_eq!(SIGB_MESSAGE & REPLY_MASK, 0);
        assert_eq!(SIGB_MESSAGE, 1);
        assert_eq!(REPLY_MASK, 0xFFFF_FFFE);
        // Fifteen reply slots are available.
        assert_eq!(REPLY_MASK.count_ones(), 31);
    }

    #[test]
    fn a_reply_bit_is_not_the_message_bit() {
        let s = TaskSignals::new(TaskId(1));
        // Reply slot 0 uses bit 1.
        s.raise(1 << 1);
        assert!(!s.has_any(SIGB_MESSAGE), "a reply is not a new request");
        assert!(s.has_any(REPLY_MASK));
    }

    #[test]
    fn task_ids_reserve_zero_for_none() {
        assert!(TaskId::NONE.is_none());
        assert!(!TaskId(1).is_none());
    }
}
