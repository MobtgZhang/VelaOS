//! The scheduler: priorities, blocking, waking, and preemption.
//!
//! # Policy and mechanism
//!
//! The scheduler is split in two on purpose:
//!
//! * **Policy** — which task runs next, which task blocks, when a task is
//!   woken. All of it is ordinary code in this module, and all of it is
//!   unit-testable on the host.
//! * **Mechanism** — how the CPU actually stops running one task and starts
//!   another. That is the `switch_to` in [`crate::context`], and it is
//!   reachable only through the [`ContextSwitch`] trait.
//!
//! The split exists because a context switch cannot be meaningfully
//! unit-tested: the only way to observe one is to run two stacks and watch
//! them interleave, which is a test of the hardware as much as of the
//! scheduler. By hiding the mechanism behind a trait, the *policy* — which is
//! where the bugs live — can be tested exhaustively, and the mechanism gets
//! exercised once, in the kernel, by the boot smoke test.
//!
//! # Priority
//!
//! VelaOS keeps AmigaOS's convention that a *lower* number is a *higher*
//! priority, and that priority 0 is the highest. The AmigaOS range of 20
//! levels is divided down to 8 internal levels; a program that sets priority
//! 10 gets internal level 2, and one that sets priority 19 gets level 3.
//!
//! The division is deliberate: AmigaOS priorities 0–10 are "real time" and
//! 11–19 are normal, and a program that depends on the distinction between,
//! say, priority 3 and priority 7 is depending on behaviour that a
//! preemptive modern kernel does not offer anyway.
//!
//! # Preemption
//!
//! A timer tick makes the current task's time slice expire. If another task
//! at the *same* priority is runnable, the current task is preempted. A task
//! at a *higher* priority preempts immediately; a task at a *lower* priority
//! does not, because the whole point of priority is that a high-priority task
//! is not disturbed by low-priority work.
//!
//! A task in the classic domain's critical section is not preempted, which
//! is what `Forbid` means. The critical section is *bounded* — the scheduler
//! force-releases it rather than letting a missing `Permit` freeze the
//! kernel.

//! The VelaOS scheduler: tasks, priorities, and context switching.
//!
//! # Module map
//!
//! | Module | Responsibility |
//! |---|---|
//! | [`task`] | [`Task`], [`Context`], the priority [`RunQueue`] |
//! | [`context`] | The `switch_to` assembly and the entry trampoline |
//!
//! The scheduler's *policy* lives in this module's root; its *mechanism*
//! lives in [`context`]. See the root documentation for why that split
//! matters.
//!
//! # What a VelaOS task is
//!
//! A task is a thread of execution with its own kernel stack, its own user
//! stack (if it has an address space of its own), its own CR3, and a saved
//! register context. The scheduler's job is to move that context between
//! CPUs.
//!
//! # The register context
//!
//! [`Context`] holds exactly the callee-saved registers that x86-64's System V
//! ABI requires a function to preserve, plus the callee-saved XMM registers.
//! The caller-saved registers are *not* saved, because the context switch
//! happens inside a function that the compiler already treats as a call
//! boundary — the compiler spills whatever it needs around the switch.
//!
//! This is the standard approach and it is worth being explicit about why it
//! is correct: `switch_to` is called from a Rust function, so the compiler
//! saves the caller-saved registers of the *caller* across the call to
//! `switch_to` on its own. What the compiler cannot know is that the *callee*
//! (the task being resumed) had values in those registers, which is why
//! `switch_to` must restore the callee-saved set explicitly.
//!
//! # The task states
//!
//! | State | Meaning |
//! |---|---|
//! | [`TaskState::Ready`] | Runnable, waiting for the CPU |
//! | [`TaskState::Running`] | Currently on a CPU |
//! | [`TaskState::Blocked`] | Waiting for a signal or a message |
//! | [`TaskState::Terminated`] | Finished; its resources are being reclaimed |

#![no_std]

extern crate alloc;

pub mod context;
pub mod task;

use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use spin::Mutex;

use vela_kernel_exec::task::{BlockedOn, TaskId};

use crate::task::{Context, CpuState, RunQueue, Task, TaskState, PRIORITY_LEVELS};

/// The AmigaOS priority range, from 0 (highest) to 19 (lowest).
pub const AMIGA_PRIORITY_HIGHEST: u8 = 0;
/// The lowest AmigaOS priority.
pub const AMIGA_PRIORITY_LOWEST: u8 = 19;
/// The highest AmigaOS priority that is considered "real time".
///
/// Below this, a task's time slice is not honoured in the AmigaOS sense;
/// above it, the task is a normal background task.
pub const AMIGA_PRIORITY_NORMAL_FROM: u8 = 10;

/// The number of timer ticks a task runs before it is preempted.
///
/// At the APIC timer's 250 Hz this is a 20 ms quantum — short enough to feel
/// responsive, long enough that a task is not preempted in the middle of
/// every syscall.
pub const QUANTUM_TICKS: u64 = 5;

/// How the scheduler performs a context switch.
///
/// The kernel implements this with the real `switch_to`. Tests implement it
/// with a simulation, which is what makes the scheduler's policy testable
/// without running real stacks.
pub trait ContextSwitch {
    /// Switch from `from`'s context into `to`'s context.
    ///
    /// # Safety
    ///
    /// Both contexts must be valid, and they must not be the same.
    fn switch(&self, from: *mut Context, to: *const Context);
}

/// The reason the current task was taken off the CPU.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreemptReason {
    /// The task's time slice expired.
    QuantumExpired,
    /// A higher-priority task became runnable.
    HigherPriorityReady,
    /// The task yielded voluntarily.
    Yielded,
    /// The task blocked.
    Blocked,
}

/// The scheduler.
pub struct Scheduler {
    /// Every task, indexed by its slot.
    tasks: Mutex<Vec<Task>>,
    /// The run queue.
    run_queue: RunQueue,
    /// The task currently on the CPU.
    current: Mutex<Option<usize>>,
    /// The task that was running before the last switch.
    ///
    /// Kept separately because the current task's context is being written
    /// into during a switch, and reading it concurrently would race.
    previous: Mutex<Option<usize>>,
    /// The CPU state.
    cpu: CpuState,
    /// How many ticks the current task has run.
    ticks_in_slice: AtomicU64,
    /// The number of preemptions that have happened.
    preemptions: AtomicU64,
    /// Whether the scheduler has been started.
    started: AtomicBool,
}

impl core::fmt::Debug for Scheduler {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Scheduler")
            .field("tasks", &self.tasks.lock().len())
            .field("queued", &self.run_queue.len())
            .field("current", &self.current.lock())
            .field("preemptions", &self.preemptions.load(Ordering::Relaxed))
            .finish()
    }
}

impl Default for Scheduler {
    fn default() -> Self {
        Self::new()
    }
}

impl Scheduler {
    /// A scheduler with no tasks.
    pub const fn new() -> Self {
        Self {
            tasks: Mutex::new(Vec::new()),
            run_queue: RunQueue::new(),
            current: Mutex::new(None),
            previous: Mutex::new(None),
            cpu: CpuState::new(),
            ticks_in_slice: AtomicU64::new(0),
            preemptions: AtomicU64::new(0),
            started: AtomicBool::new(false),
        }
    }

    /// Convert an AmigaOS priority into an internal level.
    ///
    /// The AmigaOS range is 0–19; the internal range is 0–7. The mapping is
    /// a straight division by three, clamped — so AmigaOS 0..2 all map to
    /// internal 0, which is the "highest" band.
    pub const fn priority_from_amiga(amiga: u8) -> u8 {
        let level = amiga / 3;
        if level >= PRIORITY_LEVELS as u8 {
            PRIORITY_LEVELS as u8 - 1
        } else {
            level
        }
    }

    /// Convert an internal level back to the AmigaOS priority it represents.
    ///
    /// The inverse of [`Self::priority_from_amiga`] at the band boundaries:
    /// the lowest AmigaOS priority in each band.
    pub const fn priority_to_amiga(level: u8) -> u8 {
        let level = if level >= PRIORITY_LEVELS as u8 {
            PRIORITY_LEVELS as u8 - 1
        } else {
            level
        };
        level * 3
    }

    /// Add a task.
    ///
    /// The task starts in the [`TaskState::Ready`] state and is queued.
    /// Returns the task's slot index.
    pub fn spawn(&self, id: TaskId, name: &str, priority: u8) -> usize {
        let mut tasks = self.tasks.lock();
        let slot = tasks.len();
        tasks.push(Task::new(id, name, priority));
        self.run_queue.enqueue(slot, priority);
        slot
    }

    /// How many tasks exist.
    pub fn task_count(&self) -> usize {
        self.tasks.lock().len()
    }

    /// The slot of the task currently running.
    pub fn current(&self) -> Option<usize> {
        *self.current.lock()
    }

    /// Set the slot of the task currently running.
    ///
    /// The hosted harness uses this to run a task's body without the
    /// mechanism of a real context switch. On bare metal the slot is set by
    /// the switch itself.
    pub fn set_current(&self, slot: Option<usize>) {
        *self.current.lock() = slot;
    }

    /// Put a runnable task back on the run queue at its current priority.
    ///
    /// A task that yields has been dequeued but is still runnable, so it must
    /// be re-queued. Doing it here rather than in the caller keeps the
    /// priority bookkeeping in one place.
    pub fn requeue(&self, slot: usize) -> bool {
        let priority = match self.priority_of(slot) {
            Some(p) => p,
            None => return false,
        };
        self.run_queue.enqueue(slot, priority);
        true
    }

    /// The slot of the task that was running before the last switch.
    pub fn previous(&self) -> Option<usize> {
        *self.previous.lock()
    }

    /// A task's state.
    pub fn state_of(&self, slot: usize) -> Option<TaskState> {
        self.tasks.lock().get(slot).map(|t| t.state)
    }

    /// A task's priority.
    pub fn priority_of(&self, slot: usize) -> Option<u8> {
        self.tasks.lock().get(slot).map(|t| t.priority)
    }

    /// A task's identifier.
    ///
    /// The scheduler's slots and the Exec service's task ids are different
    /// numbering schemes, and the two must be reconcilable: a task blocked on
    /// a signal has to be found from both sides.
    pub fn task_id(&self, slot: usize) -> Option<TaskId> {
        self.tasks.lock().get(slot).map(|t| t.id)
    }

    /// How many slots exist.
    pub fn slot_count(&self) -> usize {
        self.tasks.lock().len()
    }

    /// Change a task's priority.
    ///
    /// If the task is queued, it is moved to the queue for its new priority.
    /// A running task is not re-queued — it keeps running until its next
    /// slice ends, at which point it is queued at the new level.
    pub fn set_priority(&self, slot: usize, priority: u8) -> bool {
        let priority = priority.min(crate::task::PRIORITY_LOWEST);
        let mut tasks = self.tasks.lock();
        let task = match tasks.get_mut(slot) {
            Some(t) => t,
            None => return false,
        };
        if task.priority == priority {
            return true;
        }
        task.priority = priority;
        let state = task.state;
        drop(tasks);
        // A queued task must move to its new level, or it would run at the
        // old priority forever.
        if state == TaskState::Ready {
            self.run_queue.remove(slot);
            self.run_queue.enqueue(slot, priority);
        }
        true
    }

    /// Block the current task.
    ///
    /// The task leaves the run queue and its state becomes
    /// [`TaskState::Blocked`]. It must be woken by [`Self::wake`] or
    /// [`Self::wake_all`].
    pub fn block_current(&self, _on: BlockedOn) -> Option<usize> {
        let current = (*self.current.lock())?;
        let mut tasks = self.tasks.lock();
        let task = tasks.get_mut(current)?;
        task.state = TaskState::Blocked;
        drop(tasks);
        self.run_queue.remove(current);
        Some(current)
    }

    /// Wake a blocked task.
    ///
    /// Returns `true` if the task was blocked and is now runnable. Waking a
    /// task that is already running is a no-op, not an error: a signal can
    /// arrive for a task that is not waiting, and that is normal.
    pub fn wake(&self, slot: usize) -> bool {
        let mut tasks = self.tasks.lock();
        let task = match tasks.get_mut(slot) {
            Some(t) => t,
            None => return false,
        };
        if task.state != TaskState::Blocked {
            return false;
        }
        task.state = TaskState::Ready;
        let priority = task.priority;
        drop(tasks);
        self.run_queue.enqueue(slot, priority);
        true
    }

    /// Wake every blocked task.
    ///
    /// Returns how many tasks were woken.
    pub fn wake_all(&self) -> usize {
        let mut tasks = self.tasks.lock();
        let mut woken: Vec<(usize, u8)> = Vec::new();
        for (slot, task) in tasks.iter_mut().enumerate() {
            if task.state == TaskState::Blocked {
                task.state = TaskState::Ready;
                woken.push((slot, task.priority));
            }
        }
        let count = woken.len();
        drop(tasks);
        for (slot, priority) in woken {
            self.run_queue.enqueue(slot, priority);
        }
        count
    }

    /// Terminate a task.
    ///
    /// The task leaves the run queue and cannot be woken again.
    pub fn terminate(&self, slot: usize) -> bool {
        let mut tasks = self.tasks.lock();
        let task = match tasks.get_mut(slot) {
            Some(t) => t,
            None => return false,
        };
        task.state = TaskState::Terminated;
        drop(tasks);
        self.run_queue.remove(slot);
        true
    }

    /// Pick the task that should run next.
    ///
    /// Returns `None` when nothing is runnable — the idle case, in which the
    /// CPU should halt until an interrupt arrives.
    pub fn pick_next(&self) -> Option<usize> {
        self.run_queue.dequeue()
    }

    /// The highest-priority runnable task, without removing it.
    pub fn peek_next(&self) -> Option<usize> {
        self.run_queue.peek()
    }

    /// Decide whether the current task should be preempted.
    ///
    /// Returns the reason, or `None` when the current task keeps running.
    ///
    /// The rules, in order:
    ///
    /// 1. If nothing else is runnable, keep running.
    /// 2. If another task at the *same* priority is runnable and the time
    ///    slice has expired, preempt.
    /// 3. If a task at a *higher* priority is runnable, preempt immediately,
    ///    regardless of the slice.
    ///
    /// Rule 3 is what makes priority mean something: a high-priority task
    /// does not wait for the current task's slice to end.
    pub fn should_preempt(&self, current_priority: u8) -> Option<PreemptReason> {
        let next = self.run_queue.peek()?;
        let next_priority = self.priority_of(next)?;
        if next_priority < current_priority {
            return Some(PreemptReason::HigherPriorityReady);
        }
        if next_priority == current_priority
            && self.ticks_in_slice.load(Ordering::Relaxed) >= QUANTUM_TICKS
        {
            return Some(PreemptReason::QuantumExpired);
        }
        None
    }

    /// Record a timer tick.
    ///
    /// Returns the number of ticks in the current slice.
    pub fn tick(&self) -> u64 {
        self.cpu.tick();
        self.ticks_in_slice.fetch_add(1, Ordering::Relaxed) + 1
    }

    /// Reset the time slice counter.
    pub fn reset_slice(&self) {
        self.ticks_in_slice.store(0, Ordering::Relaxed);
    }

    /// How many ticks the current task has run.
    pub fn ticks_in_slice(&self) -> u64 {
        self.ticks_in_slice.load(Ordering::Relaxed)
    }

    /// Record a preemption.
    pub fn record_preemption(&self) -> u64 {
        self.preemptions.fetch_add(1, Ordering::Relaxed) + 1
    }

    /// How many preemptions have happened.
    pub fn preemptions(&self) -> u64 {
        self.preemptions.load(Ordering::Relaxed)
    }

    /// The CPU state.
    pub fn cpu(&self) -> &CpuState {
        &self.cpu
    }

    /// The run queue.
    pub fn run_queue(&self) -> &RunQueue {
        &self.run_queue
    }

    /// Mark the scheduler as started.
    pub fn start(&self) {
        self.started.store(true, Ordering::Release);
    }

    /// True once the scheduler has started.
    pub fn is_started(&self) -> bool {
        self.started.load(Ordering::Acquire)
    }

    /// Perform a context switch from `current` to `next`.
    ///
    /// The bookkeeping — which task is current, which is previous, the slice
    /// counter — happens here, and the mechanism is delegated to
    /// [`ContextSwitch`].
    pub fn switch<S: ContextSwitch>(&self, mechanism: &S, from: usize, to: usize) {
        {
            let mut previous = self.previous.lock();
            *previous = Some(from);
        }
        {
            let mut current = self.current.lock();
            *current = Some(to);
        }
        // The counters are updated under the lock, and the lock is *released*
        // before the switch.
        //
        // Releasing it matters: the switch may not return to this code at
        // all — it jumps into another task, which will want the same lock.
        // Holding it across the switch would deadlock the machine on the
        // very first context switch.
        let (from_ctx, to_ctx) = {
            let mut tasks = self.tasks.lock();
            if let Some(task) = tasks.get_mut(from) {
                task.preemptions.fetch_add(1, Ordering::Relaxed);
            }
            if let Some(task) = tasks.get_mut(to) {
                task.times_run.fetch_add(1, Ordering::Relaxed);
            }
            // Split the borrow so that both contexts can be taken at once.
            // `split_at_mut` is the only way to get two mutable borrows out
            // of one vector.
            if from == to {
                return;
            }
            let (lower, upper) = tasks.split_at_mut(to.max(from));
            let (f, t) = if from < to {
                (&mut lower[from], &mut upper[0])
            } else {
                (&mut upper[from - to], &mut lower[0])
            };
            (&mut f.context as *mut Context, &t.context as *const Context)
        };

        // The contexts are live: both slots refer to tasks in `tasks`, which
        // was just checked. The task vector only ever grows, and it is not
        // touched between here and the switch, so the pointers stay valid.
        mechanism.switch(from_ctx, to_ctx);
    }

    /// A one-line summary, for the boot banner.
    pub fn summary(&self) -> String {
        let tasks = self.tasks.lock();
        let mut out = String::new();
        out.push_str(&alloc::format!(
            "{} tasks, {} queued",
            tasks.len(),
            self.run_queue.len()
        ));
        out
    }
}

#[cfg(test)]
extern crate std;

#[cfg(test)]
mod tests {
    use super::*;

    /// A context switch that does nothing, for testing the policy.
    struct NoopSwitch;

    impl ContextSwitch for NoopSwitch {
        fn switch(&self, _from: *mut Context, _to: *const Context) {}
    }

    #[test]
    fn the_amiga_priority_maps_into_the_internal_range() {
        // AmigaOS 0 is the highest, and must map to internal 0.
        assert_eq!(Scheduler::priority_from_amiga(0), 0);
        assert_eq!(Scheduler::priority_from_amiga(1), 0);
        assert_eq!(Scheduler::priority_from_amiga(2), 0);
        assert_eq!(Scheduler::priority_from_amiga(3), 1);
        // AmigaOS 19 is the lowest, and must map inside the range.
        assert_eq!(Scheduler::priority_from_amiga(19), 6);
        // Every AmigaOS priority maps into the internal range.
        for p in AMIGA_PRIORITY_HIGHEST..=AMIGA_PRIORITY_LOWEST {
            let level = Scheduler::priority_from_amiga(p);
            assert!(
                level < PRIORITY_LEVELS as u8,
                "priority {p} mapped out of range"
            );
        }
    }

    #[test]
    fn the_priority_mapping_is_monotonic() {
        // A higher AmigaOS priority must never map to a higher internal
        // level, or the ordering would invert.
        let mut previous = 0;
        for p in AMIGA_PRIORITY_HIGHEST..=AMIGA_PRIORITY_LOWEST {
            let level = Scheduler::priority_from_amiga(p);
            assert!(level >= previous, "priority {p} inverted the order");
            previous = level;
        }
    }

    #[test]
    fn the_mapping_round_trips_at_the_band_boundaries() {
        for level in 0..PRIORITY_LEVELS as u8 {
            let amiga = Scheduler::priority_to_amiga(level);
            assert_eq!(Scheduler::priority_from_amiga(amiga), level);
        }
    }

    #[test]
    fn a_spawned_task_is_ready_and_queued() {
        let s = Scheduler::new();
        let slot = s.spawn(TaskId(1), "a", 4);
        assert_eq!(s.task_count(), 1);
        assert_eq!(s.state_of(slot), Some(TaskState::Ready));
        assert_eq!(s.run_queue().len(), 1);
        assert_eq!(s.peek_next(), Some(slot));
    }

    #[test]
    fn the_highest_priority_task_is_picked_first() {
        let s = Scheduler::new();
        let low = s.spawn(TaskId(1), "low", 6);
        let high = s.spawn(TaskId(2), "high", 0);
        assert_eq!(s.pick_next(), Some(high));
        assert_eq!(s.pick_next(), Some(low));
        assert_eq!(s.pick_next(), None);
    }

    #[test]
    fn a_blocked_task_leaves_the_queue_and_can_be_woken() {
        let s = Scheduler::new();
        let slot = s.spawn(TaskId(1), "a", 4);
        s.block_current_for_test(slot);
        assert_eq!(s.state_of(slot), Some(TaskState::Blocked));
        assert_eq!(s.run_queue().len(), 0);
        assert!(s.wake(slot));
        assert_eq!(s.state_of(slot), Some(TaskState::Ready));
        assert_eq!(s.run_queue().len(), 1);
    }

    #[test]
    fn waking_an_already_running_task_is_a_no_op() {
        let s = Scheduler::new();
        let slot = s.spawn(TaskId(1), "a", 4);
        // The task is ready, not blocked.
        assert!(!s.wake(slot));
        // And the queue is not doubled up.
        assert_eq!(s.run_queue().len(), 1);
    }

    #[test]
    fn waking_an_unknown_task_is_refused() {
        let s = Scheduler::new();
        assert!(!s.wake(99));
    }

    #[test]
    fn wake_all_wakes_every_blocked_task() {
        let s = Scheduler::new();
        let a = s.spawn(TaskId(1), "a", 4);
        let b = s.spawn(TaskId(2), "b", 4);
        s.block_current_for_test(a);
        s.block_current_for_test(b);
        assert_eq!(s.wake_all(), 2);
        assert_eq!(s.state_of(a), Some(TaskState::Ready));
        assert_eq!(s.state_of(b), Some(TaskState::Ready));
    }

    #[test]
    fn a_terminated_task_leaves_the_queue_and_stays_dead() {
        let s = Scheduler::new();
        let slot = s.spawn(TaskId(1), "a", 4);
        assert!(s.terminate(slot));
        assert_eq!(s.state_of(slot), Some(TaskState::Terminated));
        assert_eq!(s.run_queue().len(), 0);
        // A terminated task cannot be woken back to life.
        assert!(!s.wake(slot));
    }

    #[test]
    fn a_priority_change_moves_a_queued_task() {
        let s = Scheduler::new();
        let low = s.spawn(TaskId(1), "low", 6);
        let high = s.spawn(TaskId(2), "high", 0);
        // The low-priority task is raised to the highest level.
        assert!(s.set_priority(low, 0));
        assert_eq!(s.priority_of(low), Some(0));
        // It has moved out of the low level and into the high one. Leaving it
        // in the old queue would make it run at the old priority forever.
        assert_eq!(s.run_queue().len_at(6), 0);
        assert_eq!(s.run_queue().len_at(0), 2);
        // Within the new level it goes to the back: it has not yet waited in
        // that level, so it must not jump ahead of the task that was already
        // there.
        assert_eq!(s.pick_next(), Some(high));
        assert_eq!(s.pick_next(), Some(low));
    }

    #[test]
    fn a_priority_change_on_an_unknown_task_is_refused() {
        let s = Scheduler::new();
        assert!(!s.set_priority(99, 0));
    }

    #[test]
    fn a_higher_priority_task_preempts_immediately() {
        let s = Scheduler::new();
        // The high-priority task is queued and ready.
        s.spawn(TaskId(1), "high", 0);
        // The current task is at normal priority and a high-priority task is
        // ready. The slice has not expired, but priority wins.
        assert_eq!(
            s.should_preempt(4),
            Some(PreemptReason::HigherPriorityReady)
        );
    }

    #[test]
    fn a_same_priority_task_waits_for_the_quantum_to_expire() {
        let s = Scheduler::new();
        s.spawn(TaskId(1), "peer", 4);
        // The slice has not expired yet.
        assert_eq!(s.should_preempt(4), None);
        // Now it has.
        for _ in 0..QUANTUM_TICKS {
            s.tick();
        }
        assert_eq!(s.should_preempt(4), Some(PreemptReason::QuantumExpired));
    }

    #[test]
    fn a_lower_priority_task_never_preempts() {
        let s = Scheduler::new();
        s.spawn(TaskId(1), "low", 6);
        // Let the slice expire many times over.
        for _ in 0..QUANTUM_TICKS * 4 {
            s.tick();
        }
        // A low-priority task must not disturb a higher-priority one, even
        // when the higher-priority one has overrun its slice.
        assert_eq!(s.should_preempt(4), None);
    }

    #[test]
    fn a_task_is_not_preempted_when_nothing_else_is_runnable() {
        let s = Scheduler::new();
        for _ in 0..QUANTUM_TICKS * 4 {
            s.tick();
        }
        // Nothing else is ready, so the current task keeps running.
        assert_eq!(s.should_preempt(4), None);
    }

    #[test]
    fn the_slice_counter_resets() {
        let s = Scheduler::new();
        s.tick();
        s.tick();
        assert_eq!(s.ticks_in_slice(), 2);
        s.reset_slice();
        assert_eq!(s.ticks_in_slice(), 0);
    }

    #[test]
    fn preemptions_are_counted() {
        let s = Scheduler::new();
        assert_eq!(s.preemptions(), 0);
        s.record_preemption();
        s.record_preemption();
        assert_eq!(s.preemptions(), 2);
    }

    #[test]
    fn a_switch_updates_the_current_and_previous_tasks() {
        let s = Scheduler::new();
        let a = s.spawn(TaskId(1), "a", 4);
        let b = s.spawn(TaskId(2), "b", 4);
        let switch = NoopSwitch;
        s.switch(&switch, a, b);
        assert_eq!(s.current(), Some(b));
        assert_eq!(s.previous(), Some(a));
    }

    #[test]
    fn a_switch_counts_the_run_and_the_preemption() {
        let s = Scheduler::new();
        let a = s.spawn(TaskId(1), "a", 4);
        let b = s.spawn(TaskId(2), "b", 4);
        let switch = NoopSwitch;
        s.switch(&switch, a, b);
        let tasks = s.tasks.lock();
        assert_eq!(tasks[b].times_run(), 1);
        assert_eq!(tasks[a].preemptions(), 1);
    }

    #[test]
    fn the_scheduler_starts_unstarted() {
        let s = Scheduler::new();
        assert!(!s.is_started());
        s.start();
        assert!(s.is_started());
    }

    #[test]
    fn the_summary_mentions_the_task_counts() {
        let s = Scheduler::new();
        s.spawn(TaskId(1), "a", 4);
        let text = s.summary();
        assert!(text.contains("1 tasks"));
        assert!(text.contains("queued"));
    }

    impl Scheduler {
        /// Block a specific task, for tests that need to block a task that is
        /// not the current one.
        fn block_current_for_test(&self, slot: usize) {
            let mut tasks = self.tasks.lock();
            if let Some(task) = tasks.get_mut(slot) {
                task.state = TaskState::Blocked;
            }
            drop(tasks);
            self.run_queue.remove(slot);
        }
    }
}
