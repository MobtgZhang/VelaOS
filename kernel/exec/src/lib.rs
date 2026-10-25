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
use core::sync::atomic::{AtomicU32, Ordering};

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
pub mod port;
pub mod task;

use crate::library::LibraryRegistry;
use crate::port::{errors, MessageStore, PortId};
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
    /// The library registry.
    pub libraries: LibraryRegistry,
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
    /// The task the CPU is currently running, for kernels with one CPU.
    ///
    /// On bare metal the kernel is single-CPU, so one atomic in the service is
    /// exactly the hardware's answer. Hosted builds use
    /// [`current_task`]/[`Self::set_current_task`] below instead, because there
    /// a single shared value would be shared across *threads*, which stand in
    /// for CPUs.
    #[cfg(target_os = "none")]
    current: core::sync::atomic::AtomicU32,
}

impl core::fmt::Debug for Exec {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Exec")
            .field("ports", &self.ports.port_count())
            .field("libraries", &self.libraries.len())
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
            libraries: LibraryRegistry::new(),
            classic: ClassicLock::new(),
            tasks: Mutex::new(Vec::new()),
            #[cfg(target_os = "none")]
            current: core::sync::atomic::AtomicU32::new(0),
        }
    }

    /// A fresh Exec service with the built-in libraries registered.
    ///
    /// This is what the kernel and the hosted harness use. [`Self::new`] is
    /// kept as the allocation-free constructor for a `static`.
    pub fn with_builtins() -> Self {
        let exec = Self::new();
        for library in vela_abi_native64::LibraryId::ALL {
            exec.libraries.register_builtin(library);
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
    /// nowhere to go and says so rather than reaching freed memory.
    pub fn exit_task(&self, task: TaskId) -> Vec<Message> {
        let dropped = self.ports.cleanup_task(task);
        let mut tasks = self.tasks.lock();
        if task.0 >= 1 {
            let index = task.0 as usize - 1;
            if index < tasks.len() {
                tasks[index] = None;
            }
        }
        dropped
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
        self.libraries.open(name, min_version)
    }

    /// `CloseLibrary` — release a library.
    pub fn close_library(&self, handle: LibraryHandle) -> bool {
        self.libraries.close(handle)
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
        e.libraries.register("exec", 40);
        let h = e.open_library("exec.library", 40);
        assert!(!h.is_null());
        assert_eq!(e.libraries.open_count(h), 1);
        assert!(e.close_library(h));
        assert_eq!(e.libraries.open_count(h), 0);
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
