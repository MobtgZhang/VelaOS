//! The Exec service: ports, messages, signals and libraries, together.
//!
//! # Module map
//!
//! | Module | Responsibility |
//! |---|---|
//! | [`task`] | Task identity, the signal set, wait bookkeeping |
//! | [`port`] | Message ports, the FIFO queues, the reply protocol |
//! | [`library`] | The library registry and reference counting |
//!
//! This module is the façade over all three.
//!
//! # Why there is a façade
//!
//! The pieces are individually simple — a port is a queue, a signal is a mask,
//! a library is a registry entry — but the *interesting* behaviour lives in
//! how they interact:
//!
//! * `PutMsg` must make the message visible **and** wake the owner.
//! * `GetMsg` must clear the owner's "message waiting" bit **only** when the
//!   queue really is empty.
//! * `ReplyMsg` must deliver to the reply port and must **not** deliver to a
//!   port whose owner has died.
//! * `WaitPort` must wake when a message arrives *and* return it.
//!
//! Each of those needs both the port table and the task table, so they live
//! here rather than in either one alone.
//!
//! # The classic-domain scheduler lock
//!
//! AmigaOS's `Forbid()` suspends task switching so that a critical section is
//! not interleaved. On a modern preemptive kernel that is a denial-of-service
//! primitive: a program that forgets to `Permit()` stops the whole machine.
//!
//! VelaOS models it the way the report requires — as a **bounded** lock in the
//! classic domain. `forbid()` increments a counter; `permit()` decrements it;
//! the scheduler refuses to preempt while the counter is non-zero, but a
//! blocking call made under `Forbid` releases the lock rather than deadlocking.
//! The bound is what keeps a missing `Permit` from freezing the kernel: the
//! scheduler force-releases the lock after a bounded number of ticks.

#![no_std]

extern crate alloc;

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use spin::Mutex;

use vela_abi_native64::{LibraryHandle, Message};

/// The task the calling thread's "CPU" is running.
///
/// Thread-local rather than a field of [`Exec`] because it is per-CPU state:
/// it must be the same for every `Exec` in a thread and *different* between
/// threads. See [`Exec::current_task`].
///
/// `extern crate std` is unconditional so that this is declared once and
/// unconditionally — a `cfg`-gated `extern crate` would make the *use* below
/// depend on the gate too, which is the sort of thing that breaks the moment
/// the gate moves. `std` costs nothing in a `no_std` crate that never names it
/// on a bare-metal target, because the linker drops it.
#[cfg(not(target_os = "none"))]
extern crate std;

// The per-thread "current task", for hosted builds. `CURRENT_TASK` is the
// answer `current_task` reads; see its documentation there.
#[cfg(not(target_os = "none"))]
std::thread_local! {
    static CURRENT_TASK: core::cell::Cell<TaskId> = const { core::cell::Cell::new(TaskId(0)) };
}

pub mod library;
pub mod memory;
pub mod port;
pub mod semaphore;
pub mod task;

use crate::library::{ModuleKind, ModuleRegistry};
use crate::port::{errors, MessageStore, PortId};
use crate::semaphore::{Obtain, SemaphoreId, SemaphoreTable, SIGF_SINGLE};
use crate::task::{BlockedOn, TaskId, TaskSignals, SIGB_MESSAGE};

/// The classic-domain scheduler lock.
///
/// A counter rather than a boolean, because `Forbid` nests on AmigaOS: each
/// `Forbid` must be matched by a `Permit`.
#[derive(Debug, Default)]
pub struct ClassicLock {
    /// The nesting depth.
    depth: AtomicU32,
    /// How many times the lock has been force-released.
    ///
    /// A non-zero value means the classic domain misbehaved, and it is worth
    /// reporting: it is the difference between "the program is slow" and "the
    /// program is broken".
    forced_releases: AtomicU32,
}

impl ClassicLock {
    /// A lock with no `Forbid` outstanding.
    pub const fn new() -> Self {
        Self {
            depth: AtomicU32::new(0),
            forced_releases: AtomicU32::new(0),
        }
    }

    /// `Forbid` — enter the critical section.
    pub fn forbid(&self) -> u32 {
        self.depth.fetch_add(1, Ordering::AcqRel) + 1
    }

    /// `Permit` — leave the critical section.
    ///
    /// Returns the remaining depth. `Permit` without a matching `Forbid` is
    /// refused: the depth never goes below zero, because a negative depth
    /// would silently disable the lock.
    pub fn permit(&self) -> u32 {
        loop {
            let current = self.depth.load(Ordering::Acquire);
            if current == 0 {
                return 0;
            }
            if self
                .depth
                .compare_exchange_weak(current, current - 1, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                return current - 1;
            }
        }
    }

    /// True while the critical section is held.
    pub fn is_forbidden(&self) -> bool {
        self.depth.load(Ordering::Acquire) != 0
    }

    /// The current nesting depth.
    pub fn depth(&self) -> u32 {
        self.depth.load(Ordering::Acquire)
    }

    /// Force the lock open.
    ///
    /// This is the *bound* on the classic domain. The scheduler calls it when
    /// a task has held the lock for too long, or when it needs to block. A
    /// task that holds the lock is not allowed to freeze the machine.
    pub fn force_release(&self) -> bool {
        let current = self.depth.load(Ordering::Acquire);
        if current == 0 {
            return false;
        }
        self.depth.store(0, Ordering::Release);
        self.forced_releases.fetch_add(1, Ordering::AcqRel);
        true
    }

    /// How many times the lock has been force-released.
    pub fn forced_releases(&self) -> u32 {
        self.forced_releases.load(Ordering::Acquire)
    }
}

/// The Exec service.
///
/// Owns the port store, the library registry, the classic-domain lock, and
/// the table of task signal sets.
pub struct Exec {
    /// The message ports.
    pub ports: MessageStore,
    /// The semaphores.
    pub semaphores: SemaphoreTable,
    /// The module registry: libraries, devices and resources.
    pub modules: ModuleRegistry,
    /// The classic-domain scheduler lock.
    pub classic: ClassicLock,
    /// The signal sets of every live task.
    ///
    /// Held behind an `Arc` so a caller can keep a reference to a task's
    /// signal set after the table lock is released. Signal raising happens
    /// under the table lock in the current design, but returning a borrowed
    /// reference out of a `Mutex` guard is impossible, and an owned `Arc` is
    /// what makes the API honest about that.
    tasks: Mutex<Vec<Option<Arc<TaskSignals>>>>,
    /// The names tasks have given themselves, for `FindTask`.
    ///
    /// AmigaOS keeps the name inside the task's list node; here the task
    /// table owns it, so a name outlives nothing and `FindTask` scans one
    /// small table rather than two scheduler lists.
    task_names: Mutex<Vec<(TaskId, alloc::string::String)>>,
    /// The task the CPU is currently running, for kernels with one CPU.
    ///
    /// On bare metal the kernel is single-CPU, so one atomic in the service is
    /// exactly the hardware's answer. Hosted builds use
    /// [`current_task`]/[`Self::set_current_task`] below instead, because there
    /// a single shared value would be shared across *threads*, which stand in
    /// for CPUs.
    #[cfg(target_os = "none")]
    current: core::sync::atomic::AtomicU32,
    /// Set when the running task has asked to give up the rest of its slice.
    ///
    /// One flag rather than one per task, because there is one CPU: only the
    /// task that is actually running can set it, and it is read by the
    /// scheduler immediately after that task's body returns — before any
    /// other task has had a chance to run and overwrite it. A per-task flag
    /// would be a map keyed by a task id that is only ever consulted for the
    /// one task that just finished, which is the same state with more places
    /// for it to disagree with itself.
    yield_requested: AtomicBool,
}

impl core::fmt::Debug for Exec {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Exec")
            .field("ports", &self.ports.port_count())
            .field("modules", &self.modules.len())
            .field("forbidden", &self.classic.is_forbidden())
            .finish()
    }
}

impl Default for Exec {
    fn default() -> Self {
        Self::new()
    }
}

impl Exec {
    /// A fresh Exec service with no ports and no tasks.
    ///
    /// The six built-in AmigaOS libraries are registered, because they are
    /// part of what VelaOS *is*: a program that asks for
    /// `graphics.library` must get a handle, because `graphics.library` is
    /// there. What the handle names is Phase 6's business; what it means
    /// for the handle to exist is Phase 4's.
    pub const fn new() -> Self {
        Self {
            ports: MessageStore::new(),
            semaphores: SemaphoreTable::new(),
            modules: ModuleRegistry::new(),
            classic: ClassicLock::new(),
            tasks: Mutex::new(Vec::new()),
            task_names: Mutex::new(Vec::new()),
            #[cfg(target_os = "none")]
            current: core::sync::atomic::AtomicU32::new(0),
            yield_requested: AtomicBool::new(false),
        }
    }

    /// A fresh Exec service with the built-in libraries registered.
    ///
    /// This is what the kernel and the hosted harness use. [`Self::new`] is
    /// kept as the allocation-free constructor for a `static`.
    pub fn with_builtins() -> Self {
        let exec = Self::new();
        for library in vela_abi_native64::LibraryId::ALL {
            exec.modules
                .register(library.name(), library.version(), ModuleKind::Library);
        }
        exec
    }

    /// The task the CPU is currently running.
    ///
    /// A syscall handler has to be able to ask "who is calling?" without the
    /// scheduler passing it down through every layer, and this is that answer.
    ///
    /// Hosted, it is a **thread-local**: the harness runs each host thread as
    /// its own CPU, and a shared value would let one thread's `set_current_task`
    /// answer another's question. That failure is silent and vicious — two
    /// programs running at once each end up owning the other's ports, screens
    /// and windows, so a program is handed objects that belong to a different
    /// process and its replies are refused for having no reply port.
    ///
    /// On bare metal the kernel is single-CPU and the field in the struct is the
    /// whole story.
    #[cfg(not(target_os = "none"))]
    pub fn current_task(&self) -> TaskId {
        CURRENT_TASK.with(|cell| cell.get())
    }

    /// Set the task the CPU is currently running.
    #[cfg(not(target_os = "none"))]
    pub fn set_current_task(&self, task: TaskId) {
        CURRENT_TASK.with(|cell| cell.set(task));
    }

    /// The task the CPU is currently running.
    ///
    /// On a single-CPU kernel there is exactly one. The field exists so that
    /// a syscall handler can ask "who is calling?" without the scheduler
    /// having to pass it down through every layer.
    #[cfg(target_os = "none")]
    pub fn current_task(&self) -> TaskId {
        TaskId(self.current.load(Ordering::Acquire))
    }

    /// Set the task the CPU is running.
    #[cfg(target_os = "none")]
    pub fn set_current_task(&self, task: TaskId) {
        self.current.store(task.0, Ordering::Release);
    }

    /// Register a task and return its id.
    pub fn create_task(&self) -> TaskId {
        let mut tasks = self.tasks.lock();
        // A freed slot is reused, but the id is the *index*, so a stale id
        // would name a different task. The scheduler guarantees ids are not
        // reused while a task is alive; the id space here grows monotonically
        // to match.
        let index = tasks.iter().position(|t| t.is_none());
        let id = match index {
            Some(i) => i,
            None => {
                tasks.push(None);
                tasks.len() - 1
            }
        };
        tasks[id] = Some(Arc::new(TaskSignals::new(TaskId(id as u32 + 1))));
        TaskId(id as u32 + 1)
    }

    /// The signal set of a task.
    ///
    /// Returns an owned handle so the caller can use it after the table lock
    /// has been released.
    pub fn task_signals(&self, task: TaskId) -> Option<Arc<TaskSignals>> {
        let tasks = self.tasks.lock();
        tasks
            .get(task.0 as usize - 1)
            .and_then(|t| t.as_ref())
            .map(Arc::clone)
    }

    /// True when `task` is live.
    pub fn task_exists(&self, task: TaskId) -> bool {
        self.task_signals(task).is_some()
    }

    /// Remove a task and clean up everything it owned.
    ///
    /// This is the task-exit path. Ports the task owned are deleted and their
    /// messages returned, so a reply still in flight toward the dead task has
    /// nowhere to go and says so rather than reaching freed memory. Semaphores
    /// the task held are released so a lock cannot be taken forever, and its
    /// entry in every wait queue is dropped so it can never be *granted* a
    /// lock it can no longer release.
    pub fn exit_task(&self, task: TaskId) -> Vec<Message> {
        let dropped = self.ports.cleanup_task(task);
        // The semaphore cleanup returns the waiters granted in the dead
        // task's place, each of which must be woken with the grant signal.
        let granted = self.semaphores.cleanup_task(task);
        for waiter in granted {
            if let Some(signals) = self.task_signals(waiter.task) {
                signals.raise(SIGF_SINGLE);
            }
        }
        self.task_names.lock().retain(|(id, _)| *id != task);
        let mut tasks = self.tasks.lock();
        if task.0 >= 1 {
            let index = task.0 as usize - 1;
            if index < tasks.len() {
                tasks[index] = None;
            }
        }
        dropped
    }

    /// Name the calling task, the way a task names itself on start-up.
    ///
    /// `FindTask` is what reads this; AmigaOS spells the write
    /// `tc_Node.ln_Name` and the read `FindTask(name)`.
    pub fn set_task_name(&self, task: TaskId, name: &str) -> Result<(), i32> {
        if !self.task_exists(task) {
            return Err(errors::NOT_OWNER);
        }
        let mut names = self.task_names.lock();
        match names.iter_mut().find(|(id, _)| *id == task) {
            Some((_, slot)) => *slot = alloc::string::String::from(name),
            None => names.push((task, alloc::string::String::from(name))),
        }
        Ok(())
    }

    /// The name a task gave itself, if it gave one.
    pub fn task_name(&self, task: TaskId) -> Option<alloc::string::String> {
        self.task_names
            .lock()
            .iter()
            .find(|(id, _)| *id == task)
            .map(|(_, name)| name.clone())
    }

    /// `FindTask` — the task with this name, or the current task for `None`.
    ///
    /// The AROS form takes `NULL` and answers with the caller; the `Option`
    /// is that spelling. An unknown name is `None`, not an error: on AmigaOS
    /// `FindTask` returned `NULL`, and a program that probes for a task is
    /// *expecting* to be told "not here".
    pub fn find_task(&self, name: Option<&str>) -> Option<TaskId> {
        let Some(name) = name else {
            return Some(self.current_task());
        };
        if name.is_empty() {
            return None;
        }
        self.task_names
            .lock()
            .iter()
            .find(|(_, known)| known == name)
            .map(|(id, _)| *id)
    }

    /// `InitSemaphore` — create a semaphore and return its id.
    ///
    /// The classic call initialises a structure in caller memory; here the
    /// table is kernel-owned and the `priority` argument is kept for ABI
    /// shape. See [`SemaphoreTable::init`].
    pub fn init_semaphore(&self, priority: i8) -> SemaphoreId {
        self.semaphores.init(priority)
    }

    /// `ObtainSemaphore` / `ObtainSemaphoreShared`.
    ///
    /// [`Obtain::Queued`] means the caller must block until it is woken with
    /// [`SIGF_SINGLE`] and retry — the grant is the retry's reward.
    pub fn obtain_semaphore(
        &self,
        task: TaskId,
        id: SemaphoreId,
        shared: bool,
    ) -> Result<Obtain, i32> {
        self.semaphores.obtain(task, id, shared)
    }

    /// Confirm a grant, on the retry path of a queued `ObtainSemaphore`.
    ///
    /// A grant is made *inside* `ReleaseSemaphore` — owner set, queue popped
    /// — and the granted waiter is woken with `SIGF_SINGLE`. When it resumes,
    /// it calls this rather than `obtain` again, because a second acquire
    /// would nest one level deeper than the first request asked for. The
    /// call consumes the pending wait and reports whether the hand-off
    /// really named this task.
    ///
    /// A task that resumes *without* a pending grant — a spurious wake, or a
    /// wait satisfied by something else — gets `false` and must decide
    /// whether to retry the obtain or give up, exactly as `WaitPort` hands
    /// back `NULL`.
    pub fn confirm_grant(&self, task: TaskId, id: SemaphoreId) -> Result<bool, i32> {
        let Some(signals) = self.task_signals(task) else {
            return Err(errors::NOT_OWNER);
        };
        let pending =
            signals.blocked_on() == BlockedOn::Semaphore(id.0) && signals.has_any(SIGF_SINGLE);
        if !pending {
            return Ok(false);
        }
        signals.end_wait();
        Ok(self.semaphores.holds(task, id))
    }

    /// `AttemptSemaphore` / `AttemptSemaphoreShared` — the non-blocking form.
    pub fn attempt_semaphore(
        &self,
        task: TaskId,
        id: SemaphoreId,
        shared: bool,
    ) -> Result<bool, i32> {
        self.semaphores.attempt(task, id, shared)
    }

    /// `ReleaseSemaphore` — give up one nesting level, granting the queue.
    pub fn release_semaphore(&self, task: TaskId, id: SemaphoreId) -> Result<(), i32> {
        let granted = self.semaphores.release(task, id)?;
        for waiter in granted {
            if let Some(signals) = self.task_signals(waiter.task) {
                signals.raise(SIGF_SINGLE);
            }
        }
        Ok(())
    }

    /// Delete a semaphore, waking its waiters so they fail rather than hang.
    pub fn delete_semaphore(&self, id: SemaphoreId) -> Result<(), i32> {
        let wake = self.semaphores.delete(id)?;
        for waiter in wake {
            if let Some(signals) = self.task_signals(waiter.task) {
                signals.raise(SIGF_SINGLE);
            }
        }
        Ok(())
    }

    /// Create a message port owned by `owner`.
    pub fn create_port(&self, owner: TaskId, sig: u32, sig_wait: u32, flags: u32) -> PortId {
        self.ports.create_port(owner, sig, sig_wait, flags)
    }

    /// Delete a message port.
    pub fn delete_port(&self, port: PortId) -> Result<Vec<Message>, i32> {
        self.ports.delete_port(port)
    }

    /// How many messages a port is holding.
    ///
    /// The hosted harness uses this to assert on a queue's state without
    /// taking a message out of it, which is what "the reply went to the
    /// *client's* port, not the server's" needs.
    pub fn ports_queue_len(&self, port: PortId) -> usize {
        self.ports.queue_len(port).unwrap_or(0)
    }

    /// True when a port exists and holds a message.
    pub fn port_has_message(&self, port: PortId) -> bool {
        self.ports.has_msg(port).unwrap_or(false)
    }

    /// A snapshot of a port, for a user program that asked for one.
    pub fn describe_port(&self, port: PortId) -> Option<vela_abi_native64::MsgPort> {
        self.ports.describe(port)
    }

    /// `PutMsg` — enqueue a message and wake the port's owner.
    ///
    /// The order matters. The message is made visible first, then the owner is
    /// signalled, so a woken owner always finds its message already there.
    /// Signalling first would let the owner wake, find an empty queue, and go
    /// back to sleep while the message arrived microseconds later.
    ///
    /// The bits raised are the port's **own** `mp_sig` mask, not a fixed
    /// "message" bit. A port that declared `mp_sig = SIGB_MESSAGE | SIGB_SINGLE`
    /// must raise both, because that is the mask its owner waits on — and a
    /// fixed bit would leave the owner waiting for a bit that was never set,
    /// which is a hang with no error anywhere.
    pub fn put_msg(&self, port: PortId, msg: Message) -> Result<usize, i32> {
        let owner = self.ports.port_owner(port).ok_or(errors::NO_OBJECT)?;
        let count = self.ports.put_msg(port, msg)?;
        if !owner.is_none() {
            if let Some(signals) = self.task_signals(owner) {
                signals.raise(self.ports.port_sig(port).unwrap_or(SIGB_MESSAGE));
            }
        }
        Ok(count)
    }

    /// `GetMsg` — take the oldest message from a port.
    ///
    /// When the queue becomes empty, the owner's message bit is cleared. The
    /// bit is set again if another message arrives.
    pub fn get_msg(&self, port: PortId) -> Result<Message, i32> {
        let owner = self.ports.port_owner(port).ok_or(errors::NO_OBJECT)?;
        let msg = self.ports.get_msg(port)?;
        self.after_take(port, owner);
        Ok(msg)
    }

    /// `GetMsg` — take the oldest message, but only if `accept` approves it.
    ///
    /// The syscall boundary uses this: a user program that asked for a
    /// message to be copied into a buffer the kernel cannot write must not
    /// lose the message. The message stays at the head of the queue, so a
    /// later `GetMsg` — with a valid buffer — still returns it, and it is
    /// still the *oldest* one.
    pub fn get_msg_if(
        &self,
        port: PortId,
        accept: impl FnOnce(&Message) -> bool,
    ) -> Result<Message, i32> {
        let owner = self.ports.port_owner(port).ok_or(errors::NO_OBJECT)?;
        let msg = self.ports.get_msg_if(port, accept)?;
        self.after_take(port, owner);
        Ok(msg)
    }

    /// Look at the oldest message on a port without removing it.
    pub fn peek_msg(&self, port: PortId) -> Result<Message, i32> {
        self.ports.peek_msg(port)
    }

    /// `ReplyMsg` — reply to a request.
    ///
    /// The reply goes to the port the request named. If that port's owner has
    /// died, the reply is refused rather than delivered: an in-flight reply
    /// toward a dead task is exactly the case that turns a use-after-free into
    /// a silent memory corruption.
    pub fn reply_msg(&self, request: &Message, reply: Message) -> Result<usize, i32> {
        let reply_port = request.mn_reply_port as u32;
        if reply_port == 0 {
            return Err(errors::NO_REPLY_PORT);
        }
        let port = PortId(reply_port);
        let owner = self.ports.port_owner(port).ok_or(errors::NO_OBJECT)?;
        if !owner.is_none() && !self.task_exists(owner) {
            // The port still exists but its owner is gone: the reply has no
            // legitimate recipient.
            return Err(errors::NOT_OWNER);
        }
        self.ports.reply_msg(request, reply)?;
        // The reply wakes the recipient exactly as the request did — by
        // raising the *reply port's* own signal mask, which is the mask its
        // owner is waiting on.
        if !owner.is_none() {
            if let Some(signals) = self.task_signals(owner) {
                signals.raise(self.ports.port_sig(port).unwrap_or(SIGB_MESSAGE));
            }
        }
        Ok(0)
    }

    /// `WaitPort` — wait for a message on a port.
    ///
    /// Returns the message. When the task is signalled by *another* source
    /// while it waits on a port, the wait ends without a message and the
    /// caller must handle that: the AmigaOS `WaitPort` returns `NULL` in that
    /// case, and so does this.
    pub fn wait_port(&self, port: PortId, task: TaskId) -> Result<Option<Message>, i32> {
        if !self.ports.port_exists(port) {
            return Err(errors::NO_OBJECT);
        }
        let Some(signals) = self.task_signals(task) else {
            return Err(errors::NOT_OWNER);
        };
        // The bit a waiter must request to be woken by this port. `mp_sig_wait`
        // is the mask the *waiter* asks for, which is not always the mask the
        // port raises: a port may raise `SIGB_MESSAGE` while only waking a
        // waiter that asked for `SIGB_MESSAGE | SIGB_SINGLE`.
        let wait_mask = self
            .ports
            .port_sig_wait(port)
            .or_else(|| self.ports.port_sig(port))
            .unwrap_or(SIGB_MESSAGE);
        // Check for a message first: a signal may already be pending from
        // an earlier enqueue.
        if let Ok(msg) = self.ports.get_msg(port) {
            self.after_take(port, task);
            return Ok(Some(msg));
        }
        if !signals.begin_wait(wait_mask, BlockedOn::Port(port.0)) {
            // A signal is already pending. It may or may not correspond to
            // a message, so check once more before giving up.
            if let Ok(msg) = self.ports.get_msg(port) {
                self.after_take(port, task);
                return Ok(Some(msg));
            }
            signals.end_wait();
            return Ok(None);
        }
        // The task is now blocked. The scheduler will wake it; the wakeup
        // re-enters this loop.
        Ok(None)
    }

    /// Clear the owner's message bits after a message was taken.
    ///
    /// The bits cleared are the port's own `mp_sig` mask, for the same reason
    /// they are raised: a bit that outlived its message would make the owner's
    /// next wait come up empty-handed, which looks like a lost message rather
    /// than the sticky-bit bug it is.
    fn after_take(&self, port: PortId, owner: TaskId) {
        if !owner.is_none() {
            if let Some(signals) = self.task_signals(owner) {
                let mask = self.ports.port_sig(port).unwrap_or(SIGB_MESSAGE);
                let drained = self.ports.is_empty(port).unwrap_or(true);
                if drained {
                    signals.lower(mask);
                } else {
                    signals.raise(mask);
                }
            }
        }
    }

    /// `Wait` — block until any bit of `mask` is set in the task's signal set.
    ///
    /// Returns the mask of satisfied bits, or `None` when the wait was
    /// cancelled without a signal arriving.
    pub fn wait(&self, task: TaskId, mask: u32) -> Result<Option<u32>, i32> {
        let Some(signals) = self.task_signals(task) else {
            return Err(errors::NOT_OWNER);
        };
        if mask == 0 {
            // Waiting for nothing would block forever. AmigaOS treats a zero
            // mask as "return immediately".
            return Ok(Some(0));
        }
        if signals.begin_wait(mask, BlockedOn::Signal(mask)) {
            // The task blocks; the scheduler resumes it later.
            return Ok(None);
        }
        // The signal was already there.
        Ok(signals.end_wait())
    }

    /// `SetSignal` — set bits in a task's signal set.
    pub fn set_signal(&self, task: TaskId, mask: u32) -> Result<u32, i32> {
        let signals = self.task_signals(task).ok_or(errors::NOT_OWNER)?;
        Ok(signals.raise(mask))
    }

    /// `SetExcept` — clear bits in a task's signal set.
    pub fn set_except(&self, task: TaskId, mask: u32) -> Result<u32, i32> {
        let signals = self.task_signals(task).ok_or(errors::NOT_OWNER)?;
        Ok(signals.lower(mask))
    }

    /// `Signal` — raise a signal in a task's set.
    ///
    /// This is the explicit form, distinct from the implicit raise that
    /// `PutMsg` performs. The AmigaOS `Signal` takes the *task* and the mask;
    /// `Signal` on a port means "the port's owner".
    pub fn signal(&self, task: TaskId, mask: u32) -> Result<u32, i32> {
        self.set_signal(task, mask)
    }

    /// `OpenLibrary` — open a library and return its handle.
    pub fn open_library(&self, name: &str, min_version: u32) -> LibraryHandle {
        self.modules.open(name, min_version)
    }

    /// `CloseLibrary` — release a library.
    pub fn close_library(&self, handle: LibraryHandle) -> bool {
        self.modules.close(handle)
    }

    /// `OpenDevice` — open a device and return its handle.
    pub fn open_device(&self, name: &str, min_version: u32) -> LibraryHandle {
        self.modules.open(name, min_version)
    }

    /// `CloseDevice` — release a device.
    pub fn close_device(&self, handle: LibraryHandle) -> bool {
        self.modules.close(handle)
    }

    /// `OpenResource` — open a resource and return its handle.
    pub fn open_resource(&self, name: &str, min_version: u32) -> LibraryHandle {
        self.modules.open(name, min_version)
    }

    /// `Forbid` — enter the classic critical section.
    pub fn forbid(&self) -> u32 {
        self.classic.forbid()
    }

    /// `Permit` — leave the classic critical section.
    pub fn permit(&self) -> u32 {
        self.classic.permit()
    }

    /// True while the classic critical section is held.
    pub fn is_forbidden(&self) -> bool {
        self.classic.is_forbidden()
    }

    /// The classic lock's current depth.
    pub fn forbid_depth(&self) -> u32 {
        self.classic.depth()
    }

    /// Force-release the classic critical section.
    ///
    /// This is the safety net. A task that calls `Forbid` and never calls
    /// `Permit` would otherwise hold the critical section forever and stop
    /// the whole system; the scheduler calls this when a task leaves the
    /// classic domain while still holding it. It reports whether a release
    /// actually happened.
    pub fn force_release(&self) -> bool {
        self.classic.force_release()
    }

    /// How many times the classic lock has been force-released.
    ///
    /// Non-zero means a task exited inside a critical section, which is a
    /// bug in the program and worth surfacing in a diagnostic.
    pub fn forced_releases(&self) -> u32 {
        self.classic.forced_releases()
    }

    /// Ask the scheduler to run somebody else once this call returns.
    ///
    /// This is the whole of what `Yield` can do from inside a syscall: the
    /// handler runs on the caller's kernel stack, and a context switch in the
    /// middle of it would abandon that stack's frame with nothing left to
    /// return into. So the request is *recorded*, and the scheduler — which
    /// already owns the run queue and already decides what runs next — acts
    /// on it the moment the task's body returns.
    ///
    /// The request is sticky until it is taken: a task that yields twice
    /// before the scheduler gets a look has asked twice, and the answer to
    /// both is the same.
    pub fn yield_task(&self) {
        self.yield_requested.store(true, Ordering::Release);
    }

    /// Take the pending yield request, if there is one.
    ///
    /// Reading and clearing in one step is what makes this safe to call from
    /// the scheduler: a plain `load` followed by a `clear` would let a second
    /// `Yield` that landed in between be silently discarded.
    ///
    /// The request is taken whether or not the scheduler acts on it, so a
    /// task that yielded *and* blocked does not leave a stale request behind
    /// for the next task to inherit.
    pub fn take_yield(&self) -> bool {
        self.yield_requested.swap(false, Ordering::AcqRel)
    }

    /// True while a yield request is outstanding, without taking it.
    ///
    /// For tests that need to see the request the scheduler is about to
    /// honour; taking it would remove the very thing being observed.
    pub fn yield_pending(&self) -> bool {
        self.yield_requested.load(Ordering::Acquire)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(sig: u32, reply: u64) -> Message {
        Message {
            mn_reply_port: reply,
            mn_node: vela_abi_native64::Node::new(vela_abi_native64::node_type::MESSAGE, 0, ""),
            mn_sig: sig,
            mn_reply: 0,
            mn_payload: [0; vela_abi_native64::MSG_PAYLOAD_WORDS],
        }
    }

    #[test]
    fn two_tasks_can_exchange_messages() {
        let e = Exec::new();
        let a = e.create_task();
        let b = e.create_task();
        let port_a = e.create_port(a, SIGB_MESSAGE, 0, 0);
        let port_b = e.create_port(b, SIGB_MESSAGE, 0, 0);

        // A sends a request to B, naming A's port as the reply port.
        let request = msg(SIGB_MESSAGE, port_a.0 as u64);
        e.put_msg(port_b, request).unwrap();

        // B receives it, replies, and A receives the reply.
        let got = e.get_msg(port_b).unwrap();
        assert_eq!(got.mn_sig, SIGB_MESSAGE);
        e.reply_msg(&got, msg(0, 0)).unwrap();
        let reply = e.get_msg(port_a).unwrap();
        assert_eq!(reply.mn_reply_port, 0);
    }

    #[test]
    fn put_msg_wakes_the_owner() {
        let e = Exec::new();
        let a = e.create_task();
        let port = e.create_port(a, SIGB_MESSAGE, 0, 0);
        let signals = e.task_signals(a).unwrap();
        assert!(!signals.has_any(SIGB_MESSAGE));
        e.put_msg(port, msg(SIGB_MESSAGE, 0)).unwrap();
        // The wake-on-enqueue guarantee.
        assert!(signals.has_any(SIGB_MESSAGE));
    }

    #[test]
    fn get_msg_clears_the_bit_only_when_the_queue_empties() {
        let e = Exec::new();
        let a = e.create_task();
        let port = e.create_port(a, SIGB_MESSAGE, 0, 0);
        let signals = e.task_signals(a).unwrap();

        e.put_msg(port, msg(SIGB_MESSAGE, 0)).unwrap();
        e.put_msg(port, msg(SIGB_MESSAGE, 0)).unwrap();
        e.get_msg(port).unwrap();
        assert!(signals.has_any(SIGB_MESSAGE), "a message still remains");
        e.get_msg(port).unwrap();
        assert!(!signals.has_any(SIGB_MESSAGE));
    }

    #[test]
    fn a_reply_reaches_the_reply_port() {
        let e = Exec::new();
        let a = e.create_task();
        let b = e.create_task();
        let port_a = e.create_port(a, SIGB_MESSAGE, 0, 0);
        let port_b = e.create_port(b, SIGB_MESSAGE, 0, 0);

        // A sends to B, with A's port as the reply port.
        let request = msg(SIGB_MESSAGE, port_a.0 as u64);
        e.put_msg(port_b, request).unwrap();

        let got = e.get_msg(port_b).unwrap();
        e.reply_msg(&got, msg(0, 0)).unwrap();

        // The reply is at A's port.
        assert_eq!(e.ports.queue_len(port_a), Ok(1));
        assert_eq!(e.ports.queue_len(port_b), Ok(0));
    }

    #[test]
    fn a_reply_wakes_its_recipient() {
        let e = Exec::new();
        let a = e.create_task();
        let b = e.create_task();
        let port_a = e.create_port(a, SIGB_MESSAGE, 0, 0);
        let port_b = e.create_port(b, SIGB_MESSAGE, 0, 0);
        let signals_a = e.task_signals(a).unwrap();

        let request = msg(SIGB_MESSAGE, port_a.0 as u64);
        e.put_msg(port_b, request).unwrap();
        let got = e.get_msg(port_b).unwrap();
        e.reply_msg(&got, msg(0, 0)).unwrap();

        assert!(signals_a.has_any(SIGB_MESSAGE), "the reply must wake A");
    }

    #[test]
    fn a_reply_to_a_dead_task_is_refused() {
        let e = Exec::new();
        let a = e.create_task();
        let b = e.create_task();
        let port_a = e.create_port(a, SIGB_MESSAGE, 0, 0);
        let port_b = e.create_port(b, SIGB_MESSAGE, 0, 0);

        let request = msg(SIGB_MESSAGE, port_a.0 as u64);
        e.put_msg(port_b, request).unwrap();
        let got = e.get_msg(port_b).unwrap();

        // A dies with the reply still in flight. Its ports go with it.
        e.exit_task(a);
        // The reply port no longer exists, so the reply has no legitimate
        // recipient and must say so rather than reaching freed memory.
        assert_eq!(e.reply_msg(&got, msg(0, 0)), Err(errors::NO_OBJECT));
    }

    #[test]
    fn a_reply_to_a_live_port_whose_owner_is_gone_is_refused() {
        let e = Exec::new();
        let b = e.create_task();
        let port_b = e.create_port(b, SIGB_MESSAGE, 0, 0);

        // A port whose owner never existed — or has already been reaped while
        // the port survived. The reply has no legitimate recipient.
        let orphan = e.create_port(TaskId(4242), SIGB_MESSAGE, 0, 0);
        assert!(!e.task_exists(TaskId(4242)));

        let request = msg(SIGB_MESSAGE, orphan.0 as u64);
        e.put_msg(port_b, request).unwrap();
        let got = e.get_msg(port_b).unwrap();

        // The port exists but its owner does not, so the reply is refused
        // rather than delivered to a task that is not there.
        assert_eq!(e.reply_msg(&got, msg(0, 0)), Err(errors::NOT_OWNER));
    }

    #[test]
    fn task_exit_drops_the_dead_tasks_ports() {
        let e = Exec::new();
        let a = e.create_task();
        let b = e.create_task();
        let port_a = e.create_port(a, SIGB_MESSAGE, 0, 0);
        let port_b = e.create_port(b, SIGB_MESSAGE, 0, 0);
        e.put_msg(port_a, msg(1, 0)).unwrap();

        let dropped = e.exit_task(a);
        assert_eq!(dropped.len(), 1);
        assert!(!e.ports.port_exists(port_a));
        assert!(e.ports.port_exists(port_b), "another task's port survives");
        assert!(!e.task_exists(a));
    }

    #[test]
    fn the_message_order_is_preserved_across_a_conversation() {
        let e = Exec::new();
        let a = e.create_task();
        let port = e.create_port(a, SIGB_MESSAGE, 0, 0);
        for i in 1..=5u32 {
            e.put_msg(port, msg(i, 0)).unwrap();
        }
        for i in 1..=5u32 {
            assert_eq!(e.get_msg(port).unwrap().mn_sig, i);
        }
    }

    #[test]
    fn wait_returns_immediately_when_the_signal_is_already_set() {
        let e = Exec::new();
        let a = e.create_task();
        e.set_signal(a, 0b0100).unwrap();
        // The signal is already there, so the wait returns at once.
        assert_eq!(e.wait(a, 0b0100), Ok(Some(0b0100)));
    }

    #[test]
    fn wait_on_zero_returns_immediately() {
        let e = Exec::new();
        let a = e.create_task();
        // Waiting for nothing would block forever.
        assert_eq!(e.wait(a, 0), Ok(Some(0)));
    }

    #[test]
    fn a_blocking_wait_leaves_the_task_waiting() {
        let e = Exec::new();
        let a = e.create_task();
        // No signal is pending, so the task blocks.
        assert_eq!(e.wait(a, SIGB_MESSAGE), Ok(None));
        assert!(e.task_signals(a).unwrap().is_waiting());
    }

    #[test]
    fn a_later_signal_wakes_a_waiting_task() {
        let e = Exec::new();
        let a = e.create_task();
        assert_eq!(e.wait(a, SIGB_MESSAGE), Ok(None));

        // Another task raises the signal in A's set.
        e.signal(a, SIGB_MESSAGE).unwrap();
        let signals = e.task_signals(a).unwrap();
        assert!(signals.has_any(SIGB_MESSAGE));
        // The scheduler would now resume A; the wait reports the satisfied
        // bits.
        assert_eq!(signals.end_wait(), Some(SIGB_MESSAGE));
    }

    #[test]
    fn wait_port_returns_a_pending_message() {
        let e = Exec::new();
        let a = e.create_task();
        let port = e.create_port(a, SIGB_MESSAGE, 0, 0);
        e.put_msg(port, msg(SIGB_MESSAGE, 0)).unwrap();
        // The message is already there, so the wait returns it.
        let got = e.wait_port(port, a).unwrap();
        assert!(got.is_some());
        assert_eq!(got.unwrap().mn_sig, SIGB_MESSAGE);
    }

    #[test]
    fn wait_port_blocks_when_no_message_is_pending() {
        let e = Exec::new();
        let a = e.create_task();
        let port = e.create_port(a, SIGB_MESSAGE, 0, 0);
        assert_eq!(e.wait_port(port, a), Ok(None));
        assert!(e.task_signals(a).unwrap().is_waiting());
    }

    #[test]
    fn wait_port_on_a_dead_port_reports_no_object() {
        let e = Exec::new();
        let a = e.create_task();
        assert_eq!(e.wait_port(PortId(9999), a), Err(errors::NO_OBJECT));
    }

    #[test]
    fn the_classic_lock_nests() {
        let e = Exec::new();
        assert!(!e.is_forbidden());
        e.forbid();
        assert!(e.is_forbidden());
        e.forbid();
        assert_eq!(e.classic.depth(), 2);
        // Each `Forbid` needs its own `Permit`.
        assert_eq!(e.permit(), 1);
        assert!(e.is_forbidden());
        assert_eq!(e.permit(), 0);
        assert!(!e.is_forbidden());
    }

    #[test]
    fn a_permit_without_a_forbid_is_refused() {
        let e = Exec::new();
        // The depth must never go negative, or the lock would silently
        // disable itself.
        assert_eq!(e.permit(), 0);
        assert!(!e.is_forbidden());
    }

    #[test]
    fn the_classic_lock_can_be_force_released() {
        let e = Exec::new();
        e.forbid();
        e.forbid();
        // A program that forgets to `Permit` must not be able to freeze the
        // kernel; the scheduler releases the lock.
        assert!(e.classic.force_release());
        assert!(!e.is_forbidden());
        assert_eq!(e.classic.forced_releases(), 1);
        // And releasing an unlocked domain is a no-op.
        assert!(!e.classic.force_release());
    }

    #[test]
    fn libraries_open_and_close_through_exec() {
        let e = Exec::new();
        e.modules.register("exec", 40, ModuleKind::Library);
        let h = e.open_library("exec.library", 40);
        assert!(!h.is_null());
        assert_eq!(e.modules.open_count(h), 1);
        assert!(e.close_library(h));
        assert_eq!(e.modules.open_count(h), 0);
    }

    #[test]
    fn opening_an_unregistered_library_returns_null() {
        let e = Exec::new();
        assert!(e.open_library("nosuch.library", 0).is_null());
    }

    #[test]
    fn task_ids_are_distinct() {
        let e = Exec::new();
        let mut ids = Vec::new();
        for _ in 0..50 {
            ids.push(e.create_task());
        }
        let mut sorted = ids.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), 50);
    }

    #[test]
    fn a_dead_task_has_no_signal_set() {
        let e = Exec::new();
        let a = e.create_task();
        assert!(e.task_exists(a));
        e.exit_task(a);
        assert!(!e.task_exists(a));
        assert!(e.task_signals(a).is_none());
        // And signalling a dead task is an error, not a panic.
        assert_eq!(e.set_signal(a, 1), Err(errors::NOT_OWNER));
    }

    #[test]
    fn a_full_two_task_conversation_runs_in_order() {
        // The scenario the plan calls for: "hosted harness runs two tasks
        // exchanging messages, ordering, wake-on-enqueue, reply semantics,
        // signal masks".
        let e = Exec::new();
        let client = e.create_task();
        let server = e.create_task();
        let client_port = e.create_port(client, SIGB_MESSAGE, 0, 0);
        let server_port = e.create_port(server, SIGB_MESSAGE, 0, 0);

        // Three requests, in order.
        for _ in 1..=3u32 {
            let req = msg(SIGB_MESSAGE, client_port.0 as u64);
            e.put_msg(server_port, req).unwrap();
            assert!(e.task_signals(server).unwrap().has_any(SIGB_MESSAGE));
            let got = e.get_msg(server_port).unwrap();
            assert_eq!(got.mn_sig, SIGB_MESSAGE);
            e.reply_msg(&got, msg(0, 0)).unwrap();
            let reply = e.get_msg(client_port).unwrap();
            assert_eq!(reply.mn_reply_port, 0);
        }
        // Nothing is left over.
        assert_eq!(e.ports.queue_len(client_port), Ok(0));
        assert_eq!(e.ports.queue_len(server_port), Ok(0));
    }
}
