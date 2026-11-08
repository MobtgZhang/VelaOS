//! Symmetric multiprocessing support for the scheduler.
//!
//! # What this module adds
//!
//! The single-core [`Scheduler`] keeps one global run queue. Once more than
//! one CPU exists, that becomes a bottleneck: every context switch contends
//! on the same lock, and a task woken on one CPU cannot run on another without
//! migration. This module layers per-CPU run queues and reschedule-IPI
//! dispatch on top of [`Scheduler`] so that each CPU picks its own next task
//! and a remote CPU can be nudged to reschedule.
//!
//! # Per-CPU run queues
//!
//! Each CPU has its own run queue, indexed by CPU index
//! ([`crate::task::CpuState`] already gathers the per-CPU fields). A task is
//! queued on the CPU it was spawned on. A reschedule IPI tells a remote CPU
//! to look at its own queue at the next safe point.
//!
//! # Reschedule IPIs
//!
//! The architecture's IDT wires the reschedule vector
//! ([`vela_arch::apic::RESCHEDULE_VECTOR`]) to [`vela_arch::apic::on_reschedule_ipi`],
//! which sets a per-CPU pending flag. This module exposes
//! [`send_reschedule_ipi`] to send one and [`needs_reschedule`] /
//! [`acknowledge_reschedule`] to consume the flag.

use core::sync::atomic::{AtomicBool, Ordering};
use spin::Mutex;

use vela_arch::{self as arch, current_cpu_id};
use vela_arch::apic::{self, RESCHEDULE_VECTOR};

use crate::task::RunQueue;

/// The maximum number of CPUs supported by the fixed per-CPU arrays.
const MAX_CPUS: usize = 256;

/// Per-CPU run queues, indexed by CPU index.
///
/// Each entry is `None` until the CPU has run [`init_this_cpu`]. Only a CPU
/// touches its own queue outside of initialisation, so there is no
/// cross-CPU lock contention on the hot path.
static RUN_QUEUES: [Mutex<Option<RunQueue>>; MAX_CPUS] =
    [const { Mutex::new(None) }; MAX_CPUS];

/// Whether each CPU has initialised its run queue.
static CPU_INIT: [AtomicBool; MAX_CPUS] = [const { AtomicBool::new(false) }; MAX_CPUS];

/// Initialise this CPU's run queue.
///
/// Called once per CPU during early boot, before the scheduler considers it
/// runnable.
pub fn init_this_cpu() {
    let cpu = current_cpu_id();
    if cpu >= MAX_CPUS {
        return;
    }
    let mut slot = RUN_QUEUES[cpu].lock();
    if slot.is_none() {
        *slot = Some(RunQueue::new());
    }
    CPU_INIT[cpu].store(true, Ordering::Release);
}

/// True when this CPU has initialised its run queue.
pub fn is_cpu_initialised() -> bool {
    let cpu = current_cpu_id();
    cpu < MAX_CPUS && CPU_INIT[cpu].load(Ordering::Acquire)
}

/// Enqueue `task` (a task slot) on `cpu`'s run queue at `priority`.
///
/// Returns `false` when `cpu` is invalid or not yet initialised.
pub fn enqueue_on(cpu: usize, task: usize, priority: u8) -> bool {
    if cpu >= MAX_CPUS || !CPU_INIT[cpu].load(Ordering::Acquire) {
        return false;
    }
    let mut queue = RUN_QUEUES[cpu].lock();
    let Some(queue) = queue.as_mut() else { return false };
    queue.enqueue(task, priority);
    true
}

/// Pick the next task to run on `cpu`, without removing it from the queue.
pub fn peek_next_on(cpu: usize) -> Option<usize> {
    if cpu >= MAX_CPUS {
        return None;
    }
    let queue = RUN_QUEUES[cpu].lock();
    if let Some(queue) = queue.as_ref() {
        return queue.peek();
    }
    None
}

/// Dequeue and return the next task to run on `cpu`.
pub fn pick_next_on(cpu: usize) -> Option<usize> {
    if cpu >= MAX_CPUS {
        return None;
    }
    let mut queue = RUN_QUEUES[cpu].lock();
    if let Some(queue) = queue.as_mut() {
        return queue.dequeue();
    }
    None
}

/// Remove `task` from `cpu`'s run queue, if it is queued there.
pub fn remove_from_queue(cpu: usize, task: usize) -> bool {
    if cpu >= MAX_CPUS {
        return false;
    }
    let mut queue = RUN_QUEUES[cpu].lock();
    if let Some(queue) = queue.as_mut() {
        return queue.remove(task);
    }
    false
}

/// Send a reschedule IPI to `cpu`.
///
/// The target CPU's IDT has a handler for the reschedule vector that marks it
/// as needing a reschedule; the target picks a new task at its next safe
/// point.
///
/// # Safety
///
/// Writes the APIC ICR; the caller must ensure `cpu` exists and can receive
/// IPIs.
pub unsafe fn send_reschedule_ipi(cpu: usize) {
    if cpu >= MAX_CPUS {
        return;
    }
    // APIC IDs are dense and 1:1 with CPU indices on the machines VelaOS
    // targets (QEMU, and the firmware's MP table in practice).
    let apic_id = cpu as u32;
    // SAFETY: the caller guarantees the target CPU exists.
    unsafe { apic::send_ipi(apic_id, RESCHEDULE_VECTOR) };
}

/// True when this CPU has been signalled to reschedule and has not yet
/// acknowledged it.
///
/// The scheduler checks this at the next safe point (after the current syscall
/// or time slice) and picks a new task from its run queue.
pub fn needs_reschedule() -> bool {
    apic::reschedule_pending()
}

/// Acknowledge a pending reschedule on this CPU.
///
/// The scheduler calls this after it has acted on a reschedule request.
pub fn acknowledge_reschedule() {
    apic::acknowledge_reschedule();
}

/// How many CPUs have initialised run queues.
pub fn cpu_count() -> usize {
    CPU_INIT.iter().filter(|b| b.load(Ordering::Acquire)).count()
}

/// A no-op region in which a reschedule flag is consumed, keeping the
/// scheduler's critical sections small on multicore systems.
///
/// On v0.1.0 the scheduler's own lock serialises the hot path, so this is a
/// no-op; it exists so multicore dispatch has a place to hook in.
pub fn with_preemption_disabled<R>(f: impl FnOnce() -> R) -> R {
    let _ = arch::current_cpu_id();
    f()
}
