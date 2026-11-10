//! The Exec semaphores.
//!
//! # What AmigaOS semaphores are
//!
//! An AmigaOS `SignalSemaphore` is a *nested, owner-tracked, optionally
//! shared* lock with a wait queue:
//!
//! * `ObtainSemaphore` blocks until the lock is held, nesting on the caller
//!   if it already owns it (`ss_NestCount`);
//! * `AttemptSemaphore` is the non-blocking form and reports success with
//!   `TRUE`/`FALSE` rather than waiting;
//! * `ReleaseSemaphore` decrements the nest count, and when it reaches zero
//!   the lock is granted to the first waiter — or to *all* leading shared
//!   waiters — which are woken with `Signal(SIGF_SINGLE)`;
//! * a shared grant leaves `ss_Owner` empty, so further shared requests may
//!   also be granted, while exclusive requests queue behind them.
//!
//! These semantics are taken from AROS's `rom/exec/releasesemaphore.c` and
//! `rom/exec/attemptsemaphore.c`, including the queue-count bookkeeping and
//! the shared/exclusive split of the wait queue.
//!
//! # Why the bookkeeping is kernel-side
//!
//! On AmigaOS the semaphore structure lived in the caller's memory, and a
//! task that corrupted it corrupted the system. VelaOS runs programs at
//! ring 3 with an MMU between them and the kernel, so a forged `ss_*` cannot
//! be tolerated: the table of semaphores is a kernel object, the caller
//! holds an opaque id, and every access is validated against that id — the
//! same trade-off the message ports made.

use alloc::string::String;
use alloc::vec::Vec;

use spin::Mutex;

use crate::port::errors;
use crate::task::{BlockedOn, TaskId};

/// The signal a granted waiter receives.
///
/// AROS grants a semaphore by signalling the waiter with `SIGF_SINGLE`, so
/// the grant and an unrelated wakeup are distinguishable at the wait site.
pub const SIGF_SINGLE: u32 = vela_abi_native64::signals::SIGF_SINGLE;

/// An opaque semaphore identifier.
///
/// A newtype rather than a raw index so a forged id crossing the syscall
/// boundary is validated against the table rather than indexing it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SemaphoreId(pub u32);

/// A task queued on a semaphore, and the kind of grant it is waiting for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Waiter {
    /// The task that is waiting.
    pub task: TaskId,
    /// True for a shared grant, false for an exclusive one.
    pub shared: bool,
}

/// What [`SemaphoreTable::obtain`] decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Obtain {
    /// The lock is held by the caller as of this call.
    Obtained,
    /// The caller was queued; it must block and retry after `SIGF_SINGLE`.
    Queued,
}

/// The state of one semaphore.
#[derive(Debug)]
struct Semaphore {
    /// The nesting depth: how many times the current holder obtained it.
    nest_count: u32,
    /// The exclusive owner, or `None` while shared-held or free.
    owner: Option<TaskId>,
    /// The queue of tasks waiting for a grant, in arrival order.
    waiters: Vec<Waiter>,
    /// The name it is listed under in the public semaphore list.
    ///
    /// A named semaphore is one AROS's `AddSemaphore` put in the system's
    /// public list, where `FindSemaphore` can find it by name. A semaphore
    /// with no name is private to the tasks that hold its id.
    name: Option<String>,
}

impl Semaphore {
    /// A free semaphore.
    const fn new() -> Self {
        Self {
            nest_count: 0,
            owner: None,
            waiters: Vec::new(),
            name: None,
        }
    }

    /// True when the lock is currently shared-held: held by *several* tasks,
    /// none of which owns it exclusively.
    ///
    /// AROS leaves `ss_Owner` empty in this state; the nest count is what
    /// says the lock is held at all.
    fn is_shared_held(&self) -> bool {
        self.owner.is_none() && self.nest_count > 0
    }

    /// True when `task` may take an exclusive grant right now.
    fn exclusive_free(&self, task: TaskId) -> bool {
        if self.nest_count == 0 {
            return true;
        }
        // Recursive obtain: the owner nesting again.
        self.owner == Some(task)
    }

    /// True when `task` may take a shared grant right now.
    fn shared_free(&self, task: TaskId) -> bool {
        if self.nest_count == 0 || self.is_shared_held() {
            return true;
        }
        // A shared request while exclusively held by the same task nests.
        self.owner == Some(task)
    }
}

/// The table of live semaphores.
///
/// The table owns a mutex because semaphores, like ports, are reached from
/// several tasks at once: the holder obtains, the contender queues, and a
/// third task releases its own — all without ordering between them.
#[derive(Debug)]
pub struct SemaphoreTable {
    inner: Mutex<Inner>,
}

#[derive(Debug, Default)]
struct Inner {
    /// The live semaphores, indexed by id − 1, with `None` for freed slots.
    entries: Vec<Option<Semaphore>>,
    /// Freed ids, reused before the id space grows — the ports' scheme.
    free: Vec<SemaphoreId>,
}

impl Default for SemaphoreTable {
    /// A table with no semaphores.
    fn default() -> Self {
        Self::new()
    }
}

impl SemaphoreTable {
    /// A table with no semaphores.
    pub const fn new() -> Self {
        Self {
            inner: Mutex::new(Inner {
                entries: Vec::new(),
                free: Vec::new(),
            }),
        }
    }

    /// Create (`InitSemaphore`) a semaphore and return its id.
    ///
    /// The classic `priority` argument orders the wait queue when grants are
    /// made; VelaOS grants in arrival order, which is the behaviour when
    /// every semaphore is created at the same priority, so the value is
    /// recorded in the API for ABI shape but does not reorder the queue.
    pub fn init(&self, priority: i8) -> SemaphoreId {
        self.create(None, priority)
    }

    /// `AddSemaphore` — create a semaphore and list it under a name.
    ///
    /// AROS's `AddSemaphore` initialises the semaphore and enqueues it in the
    /// system's *public* semaphore list, where `FindSemaphore` looks it up
    /// by name. The same happens here: the semaphore is created, given the
    /// name, and becomes findable by every task.
    ///
    /// A name that is already taken is refused: the public list holds one
    /// semaphore per name, and two semaphores answering to one name would
    /// make `FindSemaphore` a coin toss.
    pub fn add_named(&self, name: &str, priority: i8) -> Result<SemaphoreId, i32> {
        if name.is_empty() {
            return Err(errors::INVALID_NAME);
        }
        let mut inner = self.inner.lock();
        if inner.find_by_name(name).is_some() {
            return Err(errors::NAME_TAKEN);
        }
        Ok(Self::create_locked(
            &mut inner,
            Some(String::from(name)),
            priority,
        ))
    }

    /// `FindSemaphore` — the semaphore listed under this name.
    ///
    /// The match is exact, as AmigaOS's `FindName` is: the case-insensitive
    /// search has its own name there, and a silent case-folding here would
    /// make two differently-spelled names collide.
    pub fn find_by_name(&self, name: &str) -> Option<SemaphoreId> {
        let inner = self.inner.lock();
        inner.find_by_name(name)
    }

    /// `RemSemaphore` — take a semaphore out of the public list.
    ///
    /// AROS removes the semaphore from the public list and leaves the
    /// semaphore itself alone — it belongs to the caller, who still holds it.
    /// Here the table owns the semaphore, so unlisting it clears the name:
    /// the semaphore lives on, still usable by id, but no longer findable by
    /// name.
    pub fn remove_by_name(&self, name: &str) -> Option<SemaphoreId> {
        let mut inner = self.inner.lock();
        let id = inner.find_by_name(name)?;
        if let Some(semaphore) = inner.entries[id.0 as usize - 1].as_mut() {
            semaphore.name = None;
        }
        Some(id)
    }

    /// Create a semaphore, with or without a name.
    fn create(&self, name: Option<String>, priority: i8) -> SemaphoreId {
        let mut inner = self.inner.lock();
        Self::create_locked(&mut inner, name, priority)
    }

    /// Create a semaphore in an already-locked table.
    fn create_locked(inner: &mut Inner, name: Option<String>, _priority: i8) -> SemaphoreId {
        let id = match inner.free.pop() {
            Some(id) => id,
            None => {
                let id = SemaphoreId(inner.entries.len() as u32 + 1);
                inner.entries.push(None);
                id
            }
        };
        let mut semaphore = Semaphore::new();
        semaphore.name = name;
        inner.entries[id.0 as usize - 1] = Some(semaphore);
        id
    }

    /// True when the id names a live semaphore.
    pub fn exists(&self, id: SemaphoreId) -> bool {
        id.0 >= 1
            && self
                .inner
                .lock()
                .entries
                .get(id.0 as usize - 1)
                .map(|slot| slot.is_some())
                .unwrap_or(false)
    }

    /// True when `task` exclusively holds the semaphore right now.
    ///
    /// This is how a granted waiter observes its grant: `release` performs
    /// the hand-off atomically — owner set, queue popped — and the waiter,
    /// woken by `SIGF_SINGLE`, confirms it here. Re-obtaining instead would
    /// *nest* the granted waiter one level deeper than it asked for, so the
    /// retry path is a confirmation, not a second acquire.
    pub fn holds(&self, task: TaskId, id: SemaphoreId) -> bool {
        let inner = self.inner.lock();
        match inner.semaphore(id) {
            Some(sem) => sem.owner == Some(task) && sem.nest_count > 0,
            None => false,
        }
    }

    /// `ObtainSemaphore` / `ObtainSemaphoreShared`.
    ///
    /// Grants the lock to `task` when it is free (or already `task`'s), and
    /// otherwise queues the task. A queued caller must block and retry after
    /// `SIGF_SINGLE`: [`Obtain::Queued`] says so, and the retry observes the
    /// grant that `release` made.
    pub fn obtain(&self, task: TaskId, id: SemaphoreId, shared: bool) -> Result<Obtain, i32> {
        let mut inner = self.inner.lock();
        let Some(sem) = inner.semaphore_mut(id) else {
            return Err(errors::NO_OBJECT);
        };
        let free = if shared {
            sem.shared_free(task)
        } else {
            sem.exclusive_free(task)
        };
        if free {
            grant(sem, task, shared);
            return Ok(Obtain::Obtained);
        }
        // A task that is already queued for this semaphore must not be
        // queued a second time.
        //
        // The queue holds one entry per waiting task, and a second entry
        // would grant the same task the lock twice — once for each entry —
        // so the task would end up holding two levels for one request. A
        // caller that asks again while it waits (a program polling for the
        // grant, or a scheduler that resumes it and asks again) is asking
        // the same question, not a new one.
        if sem.waiters.iter().any(|waiter| waiter.task == task) {
            return Ok(Obtain::Queued);
        }
        sem.waiters.push(Waiter { task, shared });
        Ok(Obtain::Queued)
    }

    /// `AttemptSemaphore` / `AttemptSemaphoreShared`.
    ///
    /// The non-blocking form: the lock is taken if it is free, and `false`
    /// — not an error — says the attempt failed.
    pub fn attempt(&self, task: TaskId, id: SemaphoreId, shared: bool) -> Result<bool, i32> {
        let mut inner = self.inner.lock();
        let Some(sem) = inner.semaphore_mut(id) else {
            return Err(errors::NO_OBJECT);
        };
        let free = if shared {
            sem.shared_free(task)
        } else {
            sem.exclusive_free(task)
        };
        if free {
            grant(sem, task, shared);
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// `ReleaseSemaphore`.
    ///
    /// Releases one nesting level. When the count reaches zero the lock is
    /// granted onward: to every *leading* shared waiter, or — when the first
    /// waiter is exclusive — to that waiter alone.
    ///
    /// The granted waiters are woken through `on_grant`, which is called
    /// **while the table's lock is still held**. That is not an
    /// implementation detail: the grant and the wake have to be
    /// indivisible, because a waiter that asks again between the two would
    /// find the lock already granted and take it a second time — nesting it
    /// a level deeper than it asked for. Waking the waiter under the lock
    /// closes that window.
    ///
    /// Releasing a lock the task does not hold is [`errors::NOT_OWNER`], the
    /// modern spelling of AROS's `Alert(AN_SemCorrupt)`; the alert itself is
    /// the caller's business, because an alert may need to log to a console
    /// this table cannot see.
    pub fn release(
        &self,
        task: TaskId,
        id: SemaphoreId,
        mut on_grant: impl FnMut(&Waiter),
    ) -> Result<(), i32> {
        let mut inner = self.inner.lock();
        let Some(sem) = inner.semaphore_mut(id) else {
            return Err(errors::NO_OBJECT);
        };
        // Exclusive holds are owned; shared holds are not. Releasing a lock
        // the caller neither owns nor shares is the corruption AROS alerts
        // about, so it is refused rather than unwound.
        if sem.owner.is_some() && sem.owner != Some(task) {
            return Err(errors::NOT_OWNER);
        }
        if sem.nest_count == 0 {
            return Err(errors::NOT_OWNER);
        }
        sem.nest_count -= 1;
        if sem.nest_count > 0 {
            return Ok(());
        }
        // The lock is free. Grant onward to the queue, waking each grantee
        // as it is granted.
        for waiter in grant_from_queue(sem) {
            on_grant(&waiter);
        }
        Ok(())
    }

    /// Delete a semaphore and wake its waiters.
    ///
    /// AmigaOS had no `DeleteSemaphore` — the structure lived in caller
    /// memory and simply went away. A kernel-owned table needs an explicit
    /// teardown, and a teardown that strands waiters would hang them, so
    /// the waiters are woken; they retry, see `NO_OBJECT`, and fail rather
    /// than sleep forever.
    pub fn delete(&self, id: SemaphoreId, mut on_wake: impl FnMut(&Waiter)) -> Result<(), i32> {
        let mut inner = self.inner.lock();
        let index = id.0 as usize - 1;
        if index >= inner.entries.len() || inner.entries[index].is_none() {
            return Err(errors::NO_OBJECT);
        }
        let mut sem = inner.entries[index].take().unwrap();
        inner.free.push(id);
        for waiter in core::mem::take(&mut sem.waiters) {
            on_wake(&waiter);
        }
        Ok(())
    }

    /// Release every lock `task` holds and dequeue it from every queue.
    ///
    /// This is the task-exit path — AROS's dead-task cleanup. A task that
    /// dies while holding a semaphore must not leave the lock taken forever,
    /// and a task that dies while queued must not be granted a lock it can
    /// never release. Shared holds are the one exception: they are ownerless
    /// by design, so a shared holder that dies without releasing leaves the
    /// lock shared-held — the same leak a missing `ReleaseSemaphore` is on
    /// any OS, and no worse.
    pub fn cleanup_task(&self, task: TaskId, mut on_grant: impl FnMut(&Waiter)) {
        let mut inner = self.inner.lock();
        for entry in inner.entries.iter_mut() {
            let Some(sem) = entry else { continue };
            // Release every exclusive nest level the task holds, granting
            // onward as each one empties.
            while sem.owner == Some(task) && sem.nest_count > 0 {
                sem.nest_count -= 1;
                if sem.nest_count == 0 {
                    for waiter in grant_from_queue(sem) {
                        on_grant(&waiter);
                    }
                }
            }
            // Drop the task from the wait queue.
            if let Some(pos) = sem.waiters.iter().position(|w| w.task == task) {
                sem.waiters.remove(pos);
            }
        }
    }
}

impl Inner {
    /// The live semaphore named by `id`, if there is one.
    fn semaphore_mut(&mut self, id: SemaphoreId) -> Option<&mut Semaphore> {
        self.entries
            .get_mut(id.0 as usize - 1)
            .and_then(|slot| slot.as_mut())
    }

    /// The live semaphore named by `id`, for reads.
    fn semaphore(&self, id: SemaphoreId) -> Option<&Semaphore> {
        self.entries
            .get(id.0 as usize - 1)
            .and_then(|slot| slot.as_ref())
    }

    /// The semaphore listed under `name`, if there is one.
    fn find_by_name(&self, name: &str) -> Option<SemaphoreId> {
        self.entries
            .iter()
            .enumerate()
            .find(|(_, slot)| {
                slot.as_ref()
                    .and_then(|semaphore| semaphore.name.as_deref())
                    == Some(name)
            })
            .map(|(index, _)| SemaphoreId(index as u32 + 1))
    }
}

/// Grant `task` a lock level on an already-free semaphore.
fn grant(sem: &mut Semaphore, task: TaskId, shared: bool) {
    if shared {
        // A shared grant keeps the owner empty so further shared requests
        // are also granted; the count tracks the depth.
        sem.owner = None;
    } else {
        sem.owner = Some(task);
    }
    sem.nest_count += 1;
}

/// Pop the queue's grant onto a now-free semaphore, AROS-style.
fn grant_from_queue(sem: &mut Semaphore) -> Vec<Waiter> {
    let waiters = core::mem::take(&mut sem.waiters);
    let mut granted = Vec::new();
    let mut tail = Vec::new();
    let mut all_shared = true;
    for waiter in waiters {
        if all_shared && waiter.shared {
            granted.push(waiter);
        } else if granted.is_empty() && !waiter.shared {
            granted.push(waiter);
            all_shared = false;
        } else {
            all_shared = false;
            tail.push(waiter);
        }
    }
    if granted.is_empty() {
        sem.owner = None;
    } else if granted[0].shared {
        sem.owner = None;
        sem.nest_count = granted.len() as u32;
    } else {
        sem.owner = Some(granted[0].task);
        sem.nest_count = 1;
    }
    sem.waiters = tail;
    granted
}

/// The blocked-on reason a queued waiter carries.
///
/// The scheduler and the debugger read this to tell "waiting for a grant"
/// from "waiting for a message" — the same diagnostic duty the port and
/// reply kinds of [`BlockedOn`] serve.
pub fn blocked_on_for(id: SemaphoreId) -> BlockedOn {
    BlockedOn::Semaphore(id.0)
}

/// True while a task's wait is a semaphore-grant wait.
///
/// The retry protocol works by waking the granted waiter with
/// [`SIGF_SINGLE`]; the syscall boundary uses this to tell a grant wake from
/// any other wakeup when a queued task resumes.
pub fn is_waiting_for_grant(signals: &crate::task::TaskSignals) -> bool {
    signals.is_waiting() && signals.waiting_mask() & SIGF_SINGLE != 0
}

#[cfg(test)]
extern crate std;

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    /// A table with one task, ready for use.
    struct Fixture {
        table: SemaphoreTable,
        a: TaskId,
        b: TaskId,
        c: TaskId,
        sem: SemaphoreId,
    }

    fn fixture() -> Fixture {
        let table = SemaphoreTable::new();
        let a = TaskId(1);
        let b = TaskId(2);
        let c = TaskId(3);
        let sem = table.init(0);
        Fixture {
            table,
            a,
            b,
            c,
            sem,
        }
    }

    #[test]
    fn init_creates_a_free_semaphore() {
        let f = fixture();
        assert!(f.table.exists(f.sem));
        assert!(!f.table.exists(SemaphoreId(0)));
        assert!(!f.table.exists(SemaphoreId(99)));
    }

    #[test]
    fn an_exclusive_obtain_is_immediate_on_a_free_lock() {
        let f = fixture();
        assert_eq!(f.table.obtain(f.a, f.sem, false), Ok(Obtain::Obtained));
    }

    #[test]
    fn the_owner_nests() {
        let f = fixture();
        f.table.obtain(f.a, f.sem, false).unwrap();
        assert_eq!(f.table.obtain(f.a, f.sem, false), Ok(Obtain::Obtained));
        // Two levels taken: both must be released.
        assert_eq!(f.table.release(f.a, f.sem, |_| {}), Ok(()));
        assert_eq!(f.table.release(f.a, f.sem, |_| {}), Ok(()));
        // And a third release is the corruption AROS alerts on.
        assert_eq!(f.table.release(f.a, f.sem, |_| {}), Err(errors::NOT_OWNER));
    }

    #[test]
    fn a_contender_queues_and_is_granted_on_release() {
        let f = fixture();
        f.table.obtain(f.a, f.sem, false).unwrap();
        // B queues: the lock is taken.
        assert_eq!(f.table.obtain(f.b, f.sem, false), Ok(Obtain::Queued));
        // A releases: the hand-off is atomic — B owns the lock the moment
        // the release returns, before it is even woken.
        let mut granted = Vec::new();
        f.table
            .release(f.a, f.sem, |waiter| granted.push(*waiter))
            .unwrap();
        assert_eq!(
            granted,
            vec![Waiter {
                task: f.b,
                shared: false
            }]
        );
        // B's retry confirms the grant rather than re-acquiring, because a
        // second acquire would nest one level deeper than it asked for.
        assert!(f.table.holds(f.b, f.sem));
        // And one release is enough: the lock is free again.
        assert_eq!(f.table.release(f.b, f.sem, |_| {}), Ok(()));
        assert_eq!(f.table.attempt(f.c, f.sem, false), Ok(true));
    }

    #[test]
    fn a_granted_waiter_owns_the_lock_exclusively() {
        let f = fixture();
        f.table.obtain(f.a, f.sem, false).unwrap();
        f.table.obtain(f.b, f.sem, false).unwrap();
        f.table.obtain(f.c, f.sem, false).unwrap();
        let mut granted = Vec::new();
        f.table
            .release(f.a, f.sem, |waiter| granted.push(*waiter))
            .unwrap();
        // Only the *first* waiter is granted an exclusive lock.
        assert_eq!(granted.len(), 1);
        assert_eq!(granted[0].task, f.b);
        // C is still queued: B holds the lock.
        assert_eq!(f.table.attempt(f.c, f.sem, false), Ok(false));
    }

    #[test]
    fn a_forgotten_release_cannot_be_undone_by_another_task() {
        let f = fixture();
        f.table.obtain(f.a, f.sem, false).unwrap();
        // B releasing A's lock is the corruption, refused rather than unwound.
        assert_eq!(f.table.release(f.b, f.sem, |_| {}), Err(errors::NOT_OWNER));
    }

    #[test]
    fn shared_holders_grant_each_other() {
        let f = fixture();
        f.table.obtain(f.a, f.sem, true).unwrap();
        f.table.obtain(f.b, f.sem, true).unwrap();
        // Both shared levels are live; an exclusive request must queue.
        assert_eq!(f.table.obtain(f.c, f.sem, false), Ok(Obtain::Queued));
        // One shared release leaves one held.
        assert_eq!(f.table.release(f.a, f.sem, |_| {}), Ok(()));
        assert_eq!(f.table.attempt(f.c, f.sem, false), Ok(false));
        // The last shared release grants the exclusive waiter.
        let mut granted = Vec::new();
        f.table
            .release(f.b, f.sem, |waiter| granted.push(*waiter))
            .unwrap();
        assert_eq!(
            granted,
            vec![Waiter {
                task: f.c,
                shared: false
            }]
        );
    }

    #[test]
    fn a_release_grants_every_leading_shared_waiter() {
        let f = fixture();
        f.table.obtain(f.a, f.sem, false).unwrap();
        // Two shared waiters queue behind the exclusive holder.
        f.table.obtain(f.b, f.sem, true).unwrap();
        f.table.obtain(f.c, f.sem, true).unwrap();
        let mut granted = Vec::new();
        f.table
            .release(f.a, f.sem, |waiter| granted.push(*waiter))
            .unwrap();
        assert_eq!(granted.len(), 2);
        assert!(granted.iter().all(|w| w.shared));
        // Both hold a shared level: another shared request is granted too.
        assert_eq!(f.table.attempt(f.a, f.sem, true), Ok(true));
    }

    #[test]
    fn an_exclusive_waiter_after_a_shared_one_waits_for_the_next_release() {
        let f = fixture();
        f.table.obtain(f.a, f.sem, false).unwrap();
        f.table.obtain(f.b, f.sem, true).unwrap();
        f.table.obtain(f.c, f.sem, false).unwrap();
        let mut granted = Vec::new();
        f.table
            .release(f.a, f.sem, |waiter| granted.push(*waiter))
            .unwrap();
        // B (shared) is granted; C (exclusive) stays queued.
        assert_eq!(granted.len(), 1);
        assert_eq!(granted[0].task, f.b);
        // Releasing B's shared level grants C exclusively.
        let mut granted = Vec::new();
        f.table
            .release(f.b, f.sem, |waiter| granted.push(*waiter))
            .unwrap();
        assert_eq!(
            granted,
            vec![Waiter {
                task: f.c,
                shared: false
            }]
        );
    }

    #[test]
    fn a_released_semaphore_with_no_waiters_is_free() {
        let f = fixture();
        f.table.obtain(f.a, f.sem, false).unwrap();
        assert_eq!(f.table.release(f.a, f.sem, |_| {}), Ok(()));
        assert_eq!(f.table.attempt(f.b, f.sem, false), Ok(true));
    }

    #[test]
    fn an_attempt_on_a_taken_lock_reports_false() {
        let f = fixture();
        f.table.obtain(f.a, f.sem, false).unwrap();
        assert_eq!(f.table.attempt(f.b, f.sem, false), Ok(false));
        // The failed attempt queued nothing.
        assert_eq!(f.table.release(f.a, f.sem, |_| {}), Ok(()));
        assert_eq!(f.table.attempt(f.c, f.sem, false), Ok(true));
    }

    #[test]
    fn a_shared_attempt_while_exclusively_held_fails() {
        let f = fixture();
        f.table.obtain(f.a, f.sem, false).unwrap();
        assert_eq!(f.table.attempt(f.b, f.sem, true), Ok(false));
    }

    #[test]
    fn a_deleted_semaphore_wakes_its_waiters() {
        let f = fixture();
        f.table.obtain(f.a, f.sem, false).unwrap();
        f.table.obtain(f.b, f.sem, false).unwrap();
        f.table.obtain(f.c, f.sem, true).unwrap();
        let mut wake = Vec::new();
        f.table.delete(f.sem, |waiter| wake.push(*waiter)).unwrap();
        assert_eq!(wake.len(), 2);
        assert_eq!(wake[0].task, f.b);
        assert!(wake[1].shared);
        // The id is gone, not zeroed: further uses say so.
        assert_eq!(f.table.obtain(f.a, f.sem, false), Err(errors::NO_OBJECT));
        // And the id is reused before the id space grows.
        let fresh = f.table.init(0);
        assert_eq!(fresh, f.sem, "the freed id is reused");
    }

    #[test]
    fn task_exit_releases_owned_locks_and_dequeues_the_task() {
        let f = fixture();
        f.table.obtain(f.a, f.sem, false).unwrap();
        f.table.obtain(f.b, f.sem, false).unwrap(); // B queues.
        f.table.obtain(f.c, f.sem, true).unwrap(); // C queues shared.
        let mut wake = Vec::new();
        f.table.cleanup_task(f.a, |waiter| wake.push(*waiter));
        // A's exit performs the same grant a release would: B takes the
        // exclusive lock and C stays queued behind it.
        assert_eq!(
            wake,
            vec![Waiter {
                task: f.b,
                shared: false
            }]
        );
        assert!(f.table.holds(f.b, f.sem));
        assert_eq!(f.table.attempt(f.c, f.sem, false), Ok(false));
        // B's release hands the lock to C.
        let mut granted = Vec::new();
        f.table
            .release(f.b, f.sem, |waiter| granted.push(*waiter))
            .unwrap();
        assert_eq!(
            granted,
            vec![Waiter {
                task: f.c,
                shared: true
            }]
        );
    }

    #[test]
    fn task_exit_unwinds_a_dead_holders_nest() {
        let f = fixture();
        f.table.obtain(f.a, f.sem, false).unwrap();
        f.table.obtain(f.a, f.sem, false).unwrap();
        f.table.obtain(f.b, f.sem, false).unwrap(); // B queues.
                                                    // A dies holding two levels. Cleanup unwinds both — the second one
                                                    // empties the nest and grants B exactly once.
        let mut wake = Vec::new();
        f.table.cleanup_task(f.a, |waiter| wake.push(*waiter));
        assert_eq!(
            wake,
            vec![Waiter {
                task: f.b,
                shared: false
            }]
        );
        // And the lock is B's.
        assert!(f.table.holds(f.b, f.sem));
        assert_eq!(f.table.attempt(f.c, f.sem, false), Ok(false));
    }

    #[test]
    fn asking_again_while_waiting_does_not_queue_the_task_twice() {
        let f = fixture();
        f.table.obtain(f.a, f.sem, false).unwrap();
        // B asks and is queued.
        assert_eq!(f.table.obtain(f.b, f.sem, false), Ok(Obtain::Queued));
        // B asks again — the same question, not a new one.
        assert_eq!(f.table.obtain(f.b, f.sem, false), Ok(Obtain::Queued));
        // A releases: B is granted the lock exactly once.
        let mut granted = Vec::new();
        f.table
            .release(f.a, f.sem, |waiter| granted.push(*waiter))
            .unwrap();
        assert_eq!(granted.len(), 1, "the task is granted once, not twice");
        // And the lock carries a single level: one release frees it.
        assert_eq!(f.table.release(f.b, f.sem, |_| {}), Ok(()));
        assert_eq!(f.table.attempt(f.c, f.sem, false), Ok(true));
    }

    #[test]
    fn a_dead_waiter_is_never_granted() {
        let f = fixture();
        f.table.obtain(f.a, f.sem, false).unwrap();
        f.table.obtain(f.b, f.sem, false).unwrap();
        // B dies while queued. A then releases: B must not be granted.
        f.table.cleanup_task(f.b, |_| {});
        let mut granted = Vec::new();
        f.table
            .release(f.a, f.sem, |waiter| granted.push(*waiter))
            .unwrap();
        assert!(granted.is_empty(), "a dead waiter is not granted");
        assert_eq!(f.table.attempt(f.c, f.sem, false), Ok(true));
    }

    #[test]
    fn the_blocked_on_encoding_names_the_semaphore() {
        let on = blocked_on_for(SemaphoreId(7));
        assert!(matches!(on, BlockedOn::Semaphore(7)));
    }

    #[test]
    fn a_wait_mask_containing_the_grant_bit_is_recognised() {
        let signals = crate::task::TaskSignals::new(TaskId(1));
        signals.begin_wait(SIGF_SINGLE, BlockedOn::Signal(SIGF_SINGLE));
        assert!(is_waiting_for_grant(&signals));
        // And an unrelated wait mask is not a semaphore wait.
        let other = crate::task::TaskSignals::new(TaskId(2));
        other.begin_wait(0b0100, BlockedOn::Signal(0b0100));
        assert!(!is_waiting_for_grant(&other));
    }

    #[test]
    fn ids_are_reused_in_the_ports_style() {
        let table = SemaphoreTable::new();
        let one = table.init(0);
        let two = table.init(0);
        table.delete(one, |_| {}).unwrap();
        assert_eq!(table.init(0), one, "the freed id is reused");
        let _ = two;
    }
}
