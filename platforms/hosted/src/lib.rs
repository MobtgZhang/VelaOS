//! The hosted mock kernel: real Exec services, simulated scheduling.
//!
//! # Why a mock kernel at all
//!
//! The kernel's *policy* — which task runs, when a task blocks, how a
//! message travels between two tasks — is ordinary logic, but it lives
//! behind a context switch that cannot be unit-tested: the only way to
//! observe one is to run two stacks and watch them interleave.
//!
//! So the policy is separated from the mechanism. The mechanism is one
//! assembly routine, exercised once by the boot smoke test. The policy is
//! exercised exhaustively, here, under plain `cargo test`.
//!
//! # What the harness provides
//!
//! * [`Kernel`] — the real [`Exec`] service, the real [`Scheduler`] policy,
//!   and the real syscall dispatch table, wired together.
//! * [`Kernel::spawn`] — a hosted task: an id, a priority, and a body.
//! * [`Kernel::run_until_idle`] — a cooperative round-robin over the run
//!   queue, so a task that blocks really does let another task run.
//!
//! # What it does not provide
//!
//! There is no real ring 3, no real address space, and no real `syscall`
//! instruction. A hosted task calls the kernel's services directly. The
//! *boundary* — the user-copy helpers and the gate — is exercised by the
//! syscall crate's own tests and by the boot smoke test.

#![no_std]

extern crate alloc;

use alloc::sync::Arc;
use alloc::vec::Vec;

use spin::Mutex;

use vela_kernel_dos::Dos;
use vela_kernel_exec::Exec;
use vela_kernel_graphics::Graphics;
use vela_kernel_intuition::Intuition;
use vela_kernel_scheduler::task::TaskState;
use vela_kernel_scheduler::Scheduler;
use vela_kernel_syscall::dispatch::{builtin_table, DispatchTable, SyscallContext};
use vela_kernel_syscall::numbers::SyscallArgs;
use vela_kernel_syscall::SyscallResult;

/// The screen the mock kernel's window manager composites onto.
///
/// 640 by 480 is the smallest geometry that fits a window with a title bar,
/// a border and a gadget in it, so a test that opens a window at the origin
/// with a sensible size has somewhere to put it.
pub const HOSTED_SCREEN_WIDTH: u32 = 640;
/// The mock screen's height.
pub const HOSTED_SCREEN_HEIGHT: u32 = 480;

/// Re-exported so a test can name a task without depending on `vela-kernel-exec`
/// directly — the harness is the thing it is talking to.
pub use vela_kernel_exec::task::TaskId;

/// The body of a hosted task.
///
/// The kernel is borrowed immutably: every service the harness offers takes
/// `&self`, because the kernel's state is already internally synchronised.
/// A task that needs to change the world changes it *through* the kernel —
/// which is the same rule the real kernel imposes.
///
/// The body is an `Arc` rather than a `Box` because a task that blocks is
/// *resumed*: the same body runs again when the task is woken. A real task
/// resumes where it left off, because it has its own stack; the harness
/// re-enters the body from the top, which is faithful for every case where
/// the body's progress is carried by the kernel's state (a queue, a signal
/// set, a port) rather than by a local variable.
///
/// `Send + Sync` because a test may drive the harness from a thread of its
/// own — a program that blocks in `WaitPort` has to be able to run
/// concurrently with whatever is feeding it events, and a body that could not
/// cross a thread boundary would make that impossible.
pub type TaskBody = Arc<dyn Fn(&Kernel) + Send + Sync>;

/// A hosted task: an identity, a priority, and a body.
///
/// The body runs on the harness's own stack, not on a kernel stack. That is
/// the whole difference between this and a real task, and it is the
/// difference the harness exists to paper over: the *policy* is real, the
/// *mechanism* is not.
pub struct Task {
    /// The task's identifier, as the Exec service knows it.
    pub id: TaskId,
    /// The task's priority. Lower is higher, as on AmigaOS.
    pub priority: u8,
    /// The task's body.
    body: TaskBody,
    /// The scheduler slot the task occupies.
    slot: usize,
}

impl core::fmt::Debug for Task {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Task")
            .field("id", &self.id)
            .field("priority", &self.priority)
            .field("slot", &self.slot)
            .finish()
    }
}

/// The mock kernel: Exec, the scheduler, and the syscall table.
pub struct Kernel {
    /// The Exec service. This is the *real* one — the same code the bare
    /// metal kernel runs.
    exec: Exec,
    /// The scheduler's policy. Also the real one.
    scheduler: Scheduler,
    /// The DOS service. This is the *real* one, over a real ramfs.
    dos: Dos,
    /// The Graphics service. This is the *real* one.
    graphics: Graphics,
    /// The Intuition service. This is the *real* one, over a real layer
    /// stack — so a test that opens a window gets a real layer with a real
    /// back buffer, and the compositing is the same code that runs on bare
    /// metal.
    intuition: Intuition,
    /// The syscall dispatch table.
    table: DispatchTable,
    /// The tasks, by slot.
    tasks: Mutex<Vec<Task>>,
    /// The number of task bodies that have run to completion.
    completed: Mutex<usize>,
}

impl core::fmt::Debug for Kernel {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Kernel")
            .field("tasks", &self.tasks.lock().len())
            .field("completed", &self.completed.lock())
            .finish()
    }
}

impl Default for Kernel {
    fn default() -> Self {
        Self::new()
    }
}

impl Kernel {
    /// A fresh mock kernel with no tasks.
    pub fn new() -> Self {
        Self::with_screen(HOSTED_SCREEN_WIDTH, HOSTED_SCREEN_HEIGHT)
    }

    /// A mock kernel whose screen is `width` by `height`.
    ///
    /// The screen size is a parameter rather than a constant because the
    /// geometry is what most of the interesting failures depend on: a window
    /// that is wider than its screen clips differently from one that is not,
    /// and a test that wants to check the clipping needs to be able to say so.
    pub fn with_screen(width: u32, height: u32) -> Self {
        Self {
            exec: Exec::with_builtins(),
            scheduler: Scheduler::new(),
            dos: Dos::new(),
            graphics: Graphics::new(),
            intuition: Intuition::new(width, height),
            table: builtin_table(),
            tasks: Mutex::new(Vec::new()),
            completed: Mutex::new(0),
        }
    }

    /// The Exec service.
    pub fn exec(&self) -> &Exec {
        &self.exec
    }

    /// The scheduler.
    pub fn scheduler(&self) -> &Scheduler {
        &self.scheduler
    }

    /// The syscall dispatch table.
    pub fn table(&self) -> &DispatchTable {
        &self.table
    }

    /// The DOS service.
    pub fn dos(&self) -> &Dos {
        &self.dos
    }

    /// The Graphics service.
    pub fn graphics(&self) -> &Graphics {
        &self.graphics
    }

    /// The Intuition service.
    pub fn intuition(&self) -> &Intuition {
        &self.intuition
    }

    /// How many tasks exist.
    pub fn task_count(&self) -> usize {
        self.tasks.lock().len()
    }

    /// How many task bodies have run to completion.
    pub fn completed_count(&self) -> usize {
        *self.completed.lock()
    }

    /// Spawn a task.
    ///
    /// The task is registered with the Exec service *and* with the
    /// scheduler, and the two ids are kept in step: the scheduler's slot
    /// maps to the Exec task id.
    pub fn spawn<F>(&self, name: &str, priority: u8, body: F) -> TaskId
    where
        F: Fn(&Kernel) + Send + Sync + 'static,
    {
        let id = self.exec.create_task();
        let slot = self.scheduler.spawn(id, name, priority);
        self.tasks.lock().push(Task {
            id,
            priority,
            body: Arc::new(body),
            slot,
        });
        id
    }

    /// The scheduler slot of a task.
    pub fn slot_of(&self, task: TaskId) -> Option<usize> {
        self.tasks
            .lock()
            .iter()
            .find(|t| t.id == task)
            .map(|t| t.slot)
    }

    /// The Exec task id in a scheduler slot.
    ///
    /// The two numbering schemes are different — the harness's tasks and any
    /// task the *test* creates with `create_task` both live in the same Exec
    /// table — so a task that needs to name another task from inside a body
    /// uses its slot rather than a captured id.
    pub fn task_id_at_slot(&self, slot: usize) -> Option<TaskId> {
        self.tasks.lock().get(slot).map(|t| t.id)
    }

    /// The task's current state.
    pub fn state_of(&self, task: TaskId) -> Option<TaskState> {
        self.slot_of(task).and_then(|s| self.scheduler.state_of(s))
    }

    /// Make `task` the current task, so that a service called on its behalf
    /// is attributed to it.
    ///
    /// This is what the bare-metal kernel does on a context switch: the Exec
    /// service's "current task" is whatever the CPU is running.
    pub fn set_current_task(&self, task: TaskId) {
        self.exec.set_current_task(task);
    }

    /// Dispatch a syscall as `task`.
    ///
    /// This is the hosted stand-in for the `syscall` instruction: the same
    /// dispatch table, the same handlers, the same user-copy validation —
    /// except that the "user pointer" is a real host address, because the
    /// host's address space *is* the user half here.
    pub fn syscall(&self, task: TaskId, number: u64, args: SyscallArgs) -> SyscallResult {
        self.set_current_task(task);
        let context = SyscallContext {
            task,
            exec: &self.exec,
            dos: &self.dos,
            graphics: &self.graphics,
            intuition: &self.intuition,
        };
        self.table.dispatch(number, args, &context)
    }

    /// Run tasks until nothing is runnable.
    ///
    /// This is the cooperative scheduler. It is *not* preemptive: a task
    /// runs until it blocks or yields, and the harness notices. That is
    /// enough to exercise every ordering rule the real scheduler enforces,
    /// because the rules are about *which* task is picked next, not about
    /// when a task is interrupted.
    ///
    /// The loop terminates when the run queue is empty. A task that blocks
    /// without arranging to be woken would stall the run — which is exactly
    /// what a real deadlock looks like, and why the harness bounds the run
    /// with a step limit.
    pub fn run_until_idle(&self) -> usize {
        self.run_until_idle_with_limit(10_000)
    }

    /// Run tasks until nothing is runnable, with a bound on the number of
    /// scheduling steps.
    ///
    /// Returns the number of steps taken. The bound is a safety net: a
    /// harness bug that re-queues a task forever should fail the test rather
    /// than hang it.
    pub fn run_until_idle_with_limit(&self, limit: usize) -> usize {
        let mut steps = 0usize;
        while steps < limit {
            self.reap_satisfied_waiters();
            let Some(slot) = self.scheduler.pick_next() else {
                break;
            };
            steps += 1;
            self.run_slot(slot);
        }
        steps
    }

    /// Wake every blocked task whose signal has arrived.
    ///
    /// This is what the bare-metal kernel's timer tick does: the signal is
    /// raised by whoever sent the message, and the scheduler — not the
    /// sender — is what notices and makes the task runnable again. Without
    /// this step, a task that blocked on a signal would stay blocked even
    /// though the signal is sitting in its set.
    fn reap_satisfied_waiters(&self) {
        let candidates: Vec<(usize, TaskId, u32)> = {
            let tasks = self.tasks.lock();
            tasks
                .iter()
                .filter(|t| {
                    self.scheduler
                        .state_of(t.slot)
                        .map(|s| s == TaskState::Blocked)
                        .unwrap_or(false)
                })
                .map(|t| (t.slot, t.id, 0u32))
                .collect()
        };
        for (slot, id, _) in candidates {
            let Some(signals) = self.exec.task_signals(id) else {
                continue;
            };
            if !signals.is_waiting() {
                continue;
            }
            let mask = signals.waiting_mask();
            if signals.has_any(mask) {
                // The signal arrived, so the task is runnable again. The
                // wait is *ended* here, which is what clears the published
                // mask — a task that is woken must not still be waiting.
                signals.end_wait();
                self.scheduler.wake(slot);
            }
        }
    }

    /// Run one task's body.
    fn run_slot(&self, slot: usize) {
        // The body is *cloned* out of the table, not moved: a task that
        // blocks is resumed by running the same body again, so the body must
        // survive the run. Cloning an `Arc` is a refcount bump, and the
        // table lock is released before the body runs, so a task that spawns
        // another task does not deadlock.
        let body = match self.tasks.lock().get(slot) {
            Some(task) => Arc::clone(&task.body),
            None => return,
        };

        self.scheduler.set_current(Some(slot));
        let id = self.scheduler.task_id(slot).unwrap_or(TaskId::NONE);
        self.set_current_task(id);

        body(self);

        // The task's body is finished. If the task did not block itself, it
        // is not runnable again.
        if self.scheduler.state_of(slot) != Some(TaskState::Blocked) {
            self.scheduler.terminate(slot);
        }
        let mut completed = self.completed.lock();
        *completed += 1;
    }

    /// Yield the current task.
    ///
    /// The task stays runnable and goes to the back of its priority level,
    /// which is what `Yield` means on AmigaOS.
    pub fn yield_now(&self) {
        let Some(slot) = self.scheduler.current() else {
            return;
        };
        if self.scheduler.state_of(slot) == Some(TaskState::Ready) {
            self.scheduler.requeue(slot);
        }
    }

    /// Block the current task on a signal mask.
    ///
    /// Returns `true` when the task really blocked, and `false` when the
    /// signal was already pending — the check-then-wait distinction the
    /// Exec service makes.
    pub fn block_current(&self, mask: u32) -> bool {
        let Some(slot) = self.scheduler.current() else {
            return false;
        };
        let Some(task) = self.scheduler.task_id(slot) else {
            return false;
        };
        let Some(signals) = self.exec.task_signals(task) else {
            return false;
        };
        if !signals.begin_wait(mask, vela_kernel_exec::task::BlockedOn::Signal(mask)) {
            // The signal was already there: the wait does not block.
            signals.end_wait();
            return false;
        }
        self.scheduler
            .block_current(vela_kernel_exec::task::BlockedOn::Signal(mask));
        true
    }

    /// Wake a task blocked on a signal.
    ///
    /// The signal is raised *before* the task is woken, so a woken task
    /// always finds the signal it was waiting for. Signalling first and
    /// waking second is the order the bare-metal kernel uses for the same
    /// reason: a woken task that finds nothing to wake up for is a lost
    /// wakeup.
    pub fn wake(&self, task: TaskId, mask: u32) -> bool {
        let Some(signals) = self.exec.task_signals(task) else {
            return false;
        };
        signals.raise(mask);
        let Some(slot) = self.slot_of(task) else {
            return false;
        };
        self.scheduler.wake(slot)
    }
}

#[cfg(test)]
extern crate std;

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::{Arc, Mutex as StdMutex};
    use std::vec;
    use vela_kernel_exec::task::SIGB_MESSAGE;

    #[test]
    fn a_fresh_kernel_has_no_tasks() {
        let k = Kernel::new();
        assert_eq!(k.task_count(), 0);
        assert_eq!(k.completed_count(), 0);
    }

    #[test]
    fn a_task_runs_to_completion() {
        let k = Kernel::new();
        let ran = Arc::new(AtomicU32::new(0));
        let r = Arc::clone(&ran);
        k.spawn("a", 4, move |_| {
            r.fetch_add(1, Ordering::SeqCst);
        });
        k.run_until_idle();
        assert_eq!(ran.load(Ordering::SeqCst), 1);
        assert_eq!(k.completed_count(), 1);
    }

    #[test]
    fn tasks_run_in_priority_order() {
        let k = Kernel::new();
        let order = Arc::new(StdMutex::new(Vec::new()));
        for (name, priority) in [("low", 6u8), ("high", 0), ("mid", 3)] {
            let o = Arc::clone(&order);
            let n = name;
            k.spawn(n, priority, move |_| {
                o.lock().unwrap().push(n);
            });
        }
        k.run_until_idle();
        assert_eq!(*order.lock().unwrap(), vec!["high", "mid", "low"]);
    }

    #[test]
    fn a_blocked_task_is_skipped_until_it_is_woken() {
        let k = Kernel::new();
        let order = Arc::new(StdMutex::new(Vec::new()));

        {
            let o = Arc::clone(&order);
            k.spawn("blocker", 4, move |k| {
                o.lock().unwrap().push("blocker");
                k.block_current(1 << 5);
            });
        }
        {
            let o = Arc::clone(&order);
            k.spawn("other", 4, move |_| {
                o.lock().unwrap().push("other");
            });
        }
        k.run_until_idle();
        // The blocker ran, blocked, and the other task ran. The blocker did
        // not run again.
        assert_eq!(*order.lock().unwrap(), vec!["blocker", "other"]);
    }

    #[test]
    fn a_woken_task_runs_again() {
        let k = Kernel::new();
        let count = Arc::new(AtomicU32::new(0));

        // The waiter blocks on a signal nobody has raised yet.
        {
            let c = Arc::clone(&count);
            k.spawn("waiter", 4, move |k| {
                c.fetch_add(1, Ordering::SeqCst);
                k.block_current(1 << 5);
            });
        }
        // The waker raises the signal and wakes the waiter.
        {
            let c = Arc::clone(&count);
            k.spawn("waker", 4, move |k| {
                c.fetch_add(1, Ordering::SeqCst);
                let other = k.tasks.lock()[0].id;
                k.wake(other, 1 << 5);
            });
        }

        let steps = k.run_until_idle_with_limit(100);
        assert!(steps <= 100);
        // The waiter ran, the waker ran and woke the waiter, and the waiter
        // ran a second time.
        assert!(
            count.load(Ordering::SeqCst) >= 3,
            "the woken task did not run again",
        );
    }

    #[test]
    fn a_message_raised_in_the_owner_is_seen_by_the_owner() {
        let k = Kernel::new();
        let exec = k.exec();
        let owner = exec.create_task();
        let port = exec.create_port(owner, SIGB_MESSAGE, 0, 0);
        exec.put_msg(port, vela_abi_native64::Message::empty())
            .unwrap();
        let signals = exec.task_signals(owner).unwrap();
        assert!(signals.has_any(SIGB_MESSAGE));
    }
}
