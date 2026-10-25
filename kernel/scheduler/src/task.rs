//! Tasks and their saved contexts.
//!
//! # What a task is
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

use alloc::collections::VecDeque;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use spin::Mutex;

use vela_kernel_exec::task::TaskId;

/// The size of a kernel stack, in bytes.
///
/// 32 KiB is generous for the kernel's own call depth — a syscall handler,
/// a page-table walk and a few nested calls — while keeping the total
/// footprint of many tasks reasonable.
pub const KERNEL_STACK_SIZE: usize = 32 * 1024;

/// The size of a user stack, in bytes.
pub const USER_STACK_SIZE: usize = 64 * 1024;

/// The number of priority levels.
///
/// AmigaOS has 20 priorities (0–19), counting down. VelaOS keeps the same
/// count so that a program's `SetTaskPri` call means what it means on
/// AmigaOS, but maps them onto a smaller internal range: 8 levels, with the
/// AmigaOS priority divided down.
pub const PRIORITY_LEVELS: usize = 8;

/// The highest priority value. Lower numbers are higher priority, as on
/// AmigaOS.
pub const PRIORITY_HIGHEST: u8 = 0;

/// The lowest priority value.
pub const PRIORITY_LOWEST: u8 = PRIORITY_LEVELS as u8 - 1;

/// The default priority of a new task.
pub const PRIORITY_NORMAL: u8 = 4;

/// A task's execution state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskState {
    /// Runnable, waiting for a CPU.
    Ready,
    /// Currently executing.
    Running,
    /// Waiting for a signal or a message.
    Blocked,
    /// Finished.
    Terminated,
}

/// The saved register context of a task.
///
/// The layout is load-bearing: it must match the order in which
/// `switch_to` pushes and pops it. See [`Context::switch`].
///
/// Only the callee-saved registers are here, for the reason explained in the
/// module documentation.
#[derive(Debug, Clone, Copy)]
#[repr(C)]
pub struct Context {
    /// The stack pointer.
    pub rsp: u64,
    /// The base pointer.
    pub rbp: u64,
    /// The callee-saved general registers.
    pub rbx: u64,
    /// `r12`–`r15`, also callee-saved.
    pub r12: u64,
    pub r13: u64,
    pub r14: u64,
    pub r15: u64,
    /// The callee-saved XMM registers.
    ///
    /// The System V ABI requires `xmm6`–`xmm15` to be preserved across a
    /// call. The kernel's own code does not use floating point, but a
    /// user task might, and a context switch that dropped them would corrupt
    /// it.
    pub xmm: [u128; 10],
}

impl Default for Context {
    fn default() -> Self {
        Self::new()
    }
}

impl Context {
    /// A zeroed context.
    ///
    /// Used for a task that has not run yet: its first entry goes through
    /// [`Context::switch_to_entry`], which sets the registers a fresh task
    /// needs.
    pub const fn new() -> Self {
        Self {
            rsp: 0,
            rbp: 0,
            rbx: 0,
            r12: 0,
            r13: 0,
            r14: 0,
            r15: 0,
            xmm: [0; 10],
        }
    }

    /// The context of a task that starts by calling `entry(arg)`.
    ///
    /// The stack pointer is set up so that the first `switch_to` into this
    /// task lands in the trampoline, which then calls `entry(arg)`.
    ///
    /// `stack_top` must be the *top* of the stack, and the stack grows down,
    /// so the first push lands just below it.
    pub fn for_entry(stack_top: u64, entry: u64, arg: u64) -> Self {
        // The ABI requires a 16-byte aligned stack at a call boundary. The
        // callee-saved registers are pushed by the switch trampoline, so the
        // alignment is fixed up here: three 8-byte slots of arguments and
        // saved registers plus a return address make the alignment land
        // correctly without a `sub`.
        let aligned = (stack_top) & !0xF;
        Self {
            // Leave room for the return address the trampoline pushes, and
            // align so the ABI's 16-byte rule holds at the first `call`.
            rsp: aligned - 16,
            rbp: 0,
            rbx: 0,
            // `r12` carries the entry point and `r13` carries the argument.
            r12: entry,
            r13: arg,
            r14: 0,
            r15: 0,
            xmm: [0; 10],
        }
    }

    /// The stack pointer this context will resume on.
    pub const fn stack_pointer(&self) -> u64 {
        self.rsp
    }
}

/// A task's identity and scheduling metadata.
#[derive(Debug)]
pub struct Task {
    /// The task's id.
    pub id: TaskId,
    /// The task's name, for diagnostics.
    pub name: alloc::string::String,
    /// The task's state.
    pub state: TaskState,
    /// The task's priority; lower is higher.
    pub priority: u8,
    /// The saved register context.
    pub context: Context,
    /// The kernel stack's base address.
    pub kernel_stack: u64,
    /// The kernel stack's size, in bytes.
    pub kernel_stack_size: usize,
    /// The user stack's base address, or `0` for a kernel-only task.
    pub user_stack: u64,
    /// The user stack's size, in bytes.
    pub user_stack_size: usize,
    /// The address space's CR3, or `0` for a kernel-only task.
    pub cr3: u64,
    /// The task's entry point.
    pub entry: u64,
    /// The argument passed to the entry point.
    pub arg: u64,
    /// How many times the task has been scheduled.
    pub times_run: AtomicU64,
    /// How many times the task has been preempted.
    pub preemptions: AtomicU64,
}

impl Task {
    /// A new, ready task.
    pub fn new(id: TaskId, name: &str, priority: u8) -> Self {
        Self {
            id,
            name: alloc::string::String::from(name),
            state: TaskState::Ready,
            priority: priority.min(PRIORITY_LOWEST),
            context: Context::new(),
            kernel_stack: 0,
            kernel_stack_size: 0,
            user_stack: 0,
            user_stack_size: 0,
            cr3: 0,
            entry: 0,
            arg: 0,
            times_run: AtomicU64::new(0),
            preemptions: AtomicU64::new(0),
        }
    }

    /// True when the task can run.
    pub fn is_runnable(&self) -> bool {
        matches!(self.state, TaskState::Ready | TaskState::Running)
    }

    /// True when the task has an address space of its own.
    pub fn is_user_task(&self) -> bool {
        self.cr3 != 0
    }

    /// How many times the task has been scheduled.
    pub fn times_run(&self) -> u64 {
        self.times_run.load(Ordering::Relaxed)
    }

    /// How many times the task has been preempted.
    pub fn preemptions(&self) -> u64 {
        self.preemptions.load(Ordering::Relaxed)
    }
}

/// A priority run queue.
///
/// One queue per priority level, each a FIFO of task indices. A task at a
/// higher priority level runs until it blocks or is preempted; only when no
/// task at that level is runnable does the scheduler look at the next one
/// down.
///
/// # Why a queue per level and not one sorted queue
///
/// A single sorted queue needs an insertion sort on every priority change,
/// which is O(n) in the number of tasks. A queue per level makes both
/// enqueue and dequeue O(1), and priority changes are rare enough that the
/// cost does not matter. The number of levels is fixed at eight, so the
/// scheduler's search for the highest runnable level is eight comparisons at
/// worst.
#[derive(Default)]
pub struct RunQueue {
    levels: Mutex<[VecDeque<usize>; PRIORITY_LEVELS]>,
    /// How many tasks are queued in total.
    count: Mutex<usize>,
}

impl core::fmt::Debug for RunQueue {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let levels = self.levels.lock();
        f.debug_struct("RunQueue")
            .field("queued", &self.count.lock())
            .field(
                "per_level",
                &levels.iter().map(|l| l.len()).collect::<Vec<_>>(),
            )
            .finish()
    }
}

impl RunQueue {
    /// An empty run queue.
    pub const fn new() -> Self {
        Self {
            levels: Mutex::new([const { VecDeque::new() }; PRIORITY_LEVELS]),
            count: Mutex::new(0),
        }
    }

    /// Add a task index to the queue at `priority`.
    ///
    /// The task must not already be queued; a double-enqueue would let the
    /// scheduler run a task twice concurrently.
    pub fn enqueue(&self, task: usize, priority: u8) {
        let level = (priority as usize).min(PRIORITY_LEVELS - 1);
        let mut levels = self.levels.lock();
        levels[level].push_back(task);
        let mut count = self.count.lock();
        *count += 1;
    }

    /// Remove and return the highest-priority queued task.
    ///
    /// Returns `None` when the queue is empty. Within one priority level the
    /// order is FIFO, so a task that has been waiting is not starved by a
    /// task that arrives later at the same level.
    pub fn dequeue(&self) -> Option<usize> {
        let mut levels = self.levels.lock();
        for level in 0..PRIORITY_LEVELS {
            if let Some(task) = levels[level].pop_front() {
                let mut count = self.count.lock();
                *count = count.saturating_sub(1);
                return Some(task);
            }
        }
        None
    }

    /// Take the highest-priority task *without* removing it.
    pub fn peek(&self) -> Option<usize> {
        let levels = self.levels.lock();
        for level in 0..PRIORITY_LEVELS {
            if let Some(task) = levels[level].front() {
                return Some(*task);
            }
        }
        None
    }

    /// How many tasks are queued.
    pub fn len(&self) -> usize {
        *self.count.lock()
    }

    /// True when nothing is queued.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// How many tasks are queued at `priority`.
    pub fn len_at(&self, priority: u8) -> usize {
        let level = (priority as usize).min(PRIORITY_LEVELS - 1);
        self.levels.lock()[level].len()
    }

    /// Remove a task from the queue.
    ///
    /// Used when a task is blocked or terminated while queued.
    pub fn remove(&self, task: usize) -> bool {
        let mut levels = self.levels.lock();
        for level in 0..PRIORITY_LEVELS {
            if let Some(pos) = levels[level].iter().position(|t| *t == task) {
                levels[level].remove(pos);
                let mut count = self.count.lock();
                *count = count.saturating_sub(1);
                return true;
            }
        }
        false
    }
}

/// A per-CPU scheduler state.
///
/// Single-core for now, but the type is per-CPU so that SMP later does not
/// require restructuring: the fields that would be per-CPU are already
/// gathered here.
pub struct CpuState {
    /// The task currently running on this CPU.
    pub current: Mutex<Option<usize>>,
    /// The run queue for this CPU.
    pub run_queue: RunQueue,
    /// The task that was running when this CPU was last interrupted.
    pub idle: AtomicU32,
    /// The number of timer ticks this CPU has taken.
    pub ticks: AtomicU64,
}

impl core::fmt::Debug for CpuState {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("CpuState")
            .field("current", &self.current.lock())
            .field("queued", &self.run_queue.len())
            .field("ticks", &self.ticks.load(Ordering::Relaxed))
            .finish()
    }
}

impl Default for CpuState {
    fn default() -> Self {
        Self::new()
    }
}

impl CpuState {
    /// A fresh, idle CPU state.
    pub const fn new() -> Self {
        Self {
            current: Mutex::new(None),
            run_queue: RunQueue::new(),
            idle: AtomicU32::new(0),
            ticks: AtomicU64::new(0),
        }
    }

    /// Record a timer tick.
    pub fn tick(&self) -> u64 {
        self.ticks.fetch_add(1, Ordering::Relaxed) + 1
    }

    /// How many timer ticks this CPU has taken.
    pub fn ticks(&self) -> u64 {
        self.ticks.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
extern crate std;

#[cfg(test)]
mod tests {
    use super::*;
    use std::string::String;

    #[test]
    fn a_new_task_is_ready() {
        let t = Task::new(TaskId(1), "worker", PRIORITY_NORMAL);
        assert_eq!(t.state, TaskState::Ready);
        assert!(t.is_runnable());
        assert_eq!(t.priority, PRIORITY_NORMAL);
        assert_eq!(t.name, String::from("worker"));
        assert!(!t.is_user_task());
    }

    #[test]
    fn a_priority_above_the_maximum_is_clamped() {
        // A caller that asks for priority 200 gets the lowest level, not a
        // panic and not an out-of-range index into the run queue.
        let t = Task::new(TaskId(1), "x", 200);
        assert_eq!(t.priority, PRIORITY_LOWEST);
    }

    #[test]
    fn a_blocked_task_is_not_runnable() {
        let mut t = Task::new(TaskId(1), "x", PRIORITY_NORMAL);
        t.state = TaskState::Blocked;
        assert!(!t.is_runnable());
        t.state = TaskState::Terminated;
        assert!(!t.is_runnable());
    }

    #[test]
    fn the_context_layout_is_stable() {
        use core::mem::{align_of, offset_of, size_of};
        // The context is pushed and popped by hand-written assembly, so its
        // layout is part of the contract.
        assert_eq!(offset_of!(Context, rsp), 0);
        assert_eq!(offset_of!(Context, rbp), 8);
        assert_eq!(offset_of!(Context, rbx), 16);
        assert_eq!(offset_of!(Context, r12), 24);
        assert_eq!(offset_of!(Context, r13), 32);
        assert_eq!(offset_of!(Context, r14), 40);
        assert_eq!(offset_of!(Context, r15), 48);
        assert_eq!(offset_of!(Context, xmm), 64);
        assert_eq!(align_of::<Context>(), 16);
        assert!(size_of::<Context>() >= 224);
    }

    #[test]
    fn a_fresh_context_is_zeroed() {
        let c = Context::new();
        assert_eq!(c.rsp, 0);
        assert_eq!(c.xmm, [0u128; 10]);
    }

    #[test]
    fn an_entry_context_carries_the_entry_and_the_argument() {
        let c = Context::for_entry(0x8000, 0xDEAD_BEEF, 0x1234);
        assert_eq!(c.r12, 0xDEAD_BEEF);
        assert_eq!(c.r13, 0x1234);
        // The stack pointer is below the top, because the stack grows down.
        assert!(c.rsp < 0x8000);
        // And it is 16-byte aligned, as the ABI requires at a call boundary.
        assert_eq!(c.rsp & 0xF, 0);
    }

    #[test]
    fn the_run_queue_is_fifo_within_a_priority() {
        let q = RunQueue::new();
        q.enqueue(1, PRIORITY_NORMAL);
        q.enqueue(2, PRIORITY_NORMAL);
        q.enqueue(3, PRIORITY_NORMAL);
        assert_eq!(q.dequeue(), Some(1));
        assert_eq!(q.dequeue(), Some(2));
        assert_eq!(q.dequeue(), Some(3));
        assert_eq!(q.dequeue(), None);
    }

    #[test]
    fn the_run_queue_prefers_higher_priorities() {
        let q = RunQueue::new();
        q.enqueue(1, PRIORITY_LOWEST);
        q.enqueue(2, PRIORITY_NORMAL);
        q.enqueue(3, PRIORITY_HIGHEST);
        // The highest priority runs first, regardless of insertion order.
        assert_eq!(q.dequeue(), Some(3));
        assert_eq!(q.dequeue(), Some(2));
        assert_eq!(q.dequeue(), Some(1));
    }

    #[test]
    fn lower_priority_numbers_win() {
        // Priority 0 is the highest, as on AmigaOS.
        const {
            assert!(PRIORITY_HIGHEST < PRIORITY_NORMAL);
            assert!(PRIORITY_NORMAL < PRIORITY_LOWEST);
        }
    }

    #[test]
    fn the_run_queue_count_tracks_the_tasks() {
        let q = RunQueue::new();
        assert!(q.is_empty());
        q.enqueue(1, PRIORITY_NORMAL);
        q.enqueue(2, PRIORITY_NORMAL);
        assert_eq!(q.len(), 2);
        assert_eq!(q.len_at(PRIORITY_NORMAL), 2);
        q.dequeue();
        assert_eq!(q.len(), 1);
    }

    #[test]
    fn a_task_can_be_removed_from_the_queue() {
        let q = RunQueue::new();
        q.enqueue(1, PRIORITY_NORMAL);
        q.enqueue(2, PRIORITY_NORMAL);
        assert!(q.remove(1));
        assert_eq!(q.dequeue(), Some(2));
        // Removing a task that is not queued is not an error.
        assert!(!q.remove(99));
    }

    #[test]
    fn peeking_does_not_remove() {
        let q = RunQueue::new();
        q.enqueue(7, PRIORITY_NORMAL);
        assert_eq!(q.peek(), Some(7));
        assert_eq!(q.peek(), Some(7));
        assert_eq!(q.len(), 1);
    }

    #[test]
    fn the_cpu_state_counts_ticks() {
        let c = CpuState::new();
        assert_eq!(c.ticks(), 0);
        assert_eq!(c.tick(), 1);
        assert_eq!(c.tick(), 2);
        assert_eq!(c.ticks(), 2);
    }

    #[test]
    fn the_priority_levels_are_a_power_of_two() {
        // A power-of-two level count lets the priority be masked rather than
        // clamped, which is one instruction instead of a branch.
        assert_eq!(PRIORITY_LEVELS & (PRIORITY_LEVELS - 1), 0);
        assert_eq!(PRIORITY_LEVELS, 8);
    }

    #[test]
    fn the_stack_sizes_are_page_multiples() {
        // Stacks are mapped as pages, so their sizes must be page multiples.
        assert_eq!(KERNEL_STACK_SIZE % 4096, 0);
        assert_eq!(USER_STACK_SIZE % 4096, 0);
    }
}
