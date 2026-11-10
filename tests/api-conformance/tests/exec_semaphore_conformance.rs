//! The `exec.library` semaphores, task names and alerts.
//!
//! These tests drive the **real** Exec service through the **real** syscall
//! dispatch table, using the hosted mock kernel — the same path a program
//! takes on hardware.
//!
//! # The protocol under test
//!
//! A semaphore grant is made *inside* `ReleaseSemaphore`: the owner changes,
//! the queue is popped, and the grantee is woken with `SIGF_SINGLE`. The
//! grantee then asks again, and the second call confirms the hand-off
//! rather than acquiring the lock a second time. The tests below follow
//! that protocol exactly, because it is the protocol the kernel implements:
//! a test that acquired twice would be testing a different system.

use vela_kernel_syscall::numbers::{self, errors, SyscallArgs};
use vela_test_api_conformance::harness;

use harness::{cstring, ok};

/// The `SIGF_SINGLE` mask a semaphore grant raises.
///
/// AmigaOS numbers it 16; the kernel's reply space is bits 1–15, so the
/// grant lands on the first bit above it.
const SIGF_SINGLE: u32 = 1 << 16;

/// The call's arguments, with the rest of the words zero.
fn args(a0: u64, a1: u64) -> SyscallArgs {
    SyscallArgs::new(a0, a1, 0, 0, 0, 0)
}

/// Create a semaphore and return its id.
fn create(k: &vela_hosted::Kernel, task: vela_kernel_exec::task::TaskId) -> u64 {
    let result = ok(
        harness::call(k, task, numbers::CREATE_SEMAPHORE, args(0, 0)),
        "CreateSemaphore",
    );
    assert_ne!(result.value, 0, "a semaphore must have an id");
    result.value
}

/// The semaphore's exclusive-obtain call.
fn obtain(k: &vela_hosted::Kernel, task: vela_kernel_exec::task::TaskId, semaphore: u64) -> u64 {
    ok(
        harness::call(k, task, numbers::OBTAIN_SEMAPHORE, args(semaphore, 0)),
        "ObtainSemaphore",
    )
    .value
}

#[test]
fn exec_semaphore_lifecycle_is_verified() {
    let k = harness::kernel();
    let t = harness::task(&k);

    let semaphore = create(&k, t);

    // A free semaphore is granted at once.
    assert_eq!(obtain(&k, t, semaphore), 1, "the lock is taken");

    // The owner nests: the second obtain succeeds too.
    assert_eq!(obtain(&k, t, semaphore), 1, "the owner nests");

    // One release per level.
    ok(
        harness::call(&k, t, numbers::RELEASE_SEMAPHORE, args(semaphore, 0)),
        "ReleaseSemaphore",
    );
    ok(
        harness::call(&k, t, numbers::RELEASE_SEMAPHORE, args(semaphore, 0)),
        "ReleaseSemaphore",
    );

    // A third release is the corruption AmigaOS alerted about.
    let extra = harness::call(&k, t, numbers::RELEASE_SEMAPHORE, args(semaphore, 0));
    assert_eq!(extra.error, errors::NOT_OWNER);

    // And the semaphore can be deleted.
    ok(
        harness::call(&k, t, numbers::DELETE_SEMAPHORE, args(semaphore, 0)),
        "DeleteSemaphore",
    );
    // A deleted semaphore says so rather than answering as if it were free.
    let gone = harness::call(&k, t, numbers::ATTEMPT_SEMAPHORE, args(semaphore, 0));
    assert_eq!(gone.error, errors::NO_OBJECT);
}

#[test]
fn exec_attempt_semaphore_reports_failure_without_blocking() {
    let k = harness::kernel();
    let owner = harness::task(&k);
    let other = harness::task(&k);

    let semaphore = create(&k, owner);
    assert_eq!(obtain(&k, owner, semaphore), 1);

    // The attempt fails — and fails with an answer, not an error, exactly
    // as AmigaOS's `AttemptSemaphore` reported.
    let attempt = ok(
        harness::call(&k, other, numbers::ATTEMPT_SEMAPHORE, args(semaphore, 0)),
        "AttemptSemaphore",
    );
    assert_eq!(attempt.value, 0, "the lock is held");

    // Once the owner lets go, the attempt succeeds.
    ok(
        harness::call(&k, owner, numbers::RELEASE_SEMAPHORE, args(semaphore, 0)),
        "ReleaseSemaphore",
    );
    let attempt = ok(
        harness::call(&k, other, numbers::ATTEMPT_SEMAPHORE, args(semaphore, 0)),
        "AttemptSemaphore",
    );
    assert_eq!(attempt.value, 1, "the lock is free now");
}

#[test]
fn exec_semaphore_grant_reaches_the_waiter() {
    let k = harness::kernel();
    let owner = harness::task(&k);
    let waiter = harness::task(&k);

    let semaphore = create(&k, owner);
    assert_eq!(obtain(&k, owner, semaphore), 1);

    // The waiter queues: the call answers "not now".
    assert_eq!(obtain(&k, waiter, semaphore), 0, "the waiter is queued");

    // The owner's release grants the lock and wakes the waiter with
    // `SIGF_SINGLE` — the signal AROS's `ReleaseSemaphore` raises.
    ok(
        harness::call(&k, owner, numbers::RELEASE_SEMAPHORE, args(semaphore, 0)),
        "ReleaseSemaphore",
    );
    let signals = k.exec().task_signals(waiter).expect("the waiter is alive");
    assert!(
        signals.has_any(SIGF_SINGLE),
        "the grant must wake the waiter with SIGF_SINGLE",
    );

    // The waiter's retry confirms the hand-off. It does *not* take the lock
    // a second time, which is why the confirmation is a separate answer.
    let confirmed = ok(
        harness::call(&k, waiter, numbers::CONFIRM_GRANT, args(semaphore, 0)),
        "ConfirmGrant",
    );
    assert_eq!(confirmed.value, 1, "the grant named the waiter");

    // A second confirmation finds nothing pending: the wait was consumed.
    let again = ok(
        harness::call(&k, waiter, numbers::CONFIRM_GRANT, args(semaphore, 0)),
        "ConfirmGrant",
    );
    assert_eq!(again.value, 0, "a grant cannot be confirmed twice");

    // And the waiter owns the lock now: the owner cannot take it.
    let stolen = ok(
        harness::call(&k, owner, numbers::ATTEMPT_SEMAPHORE, args(semaphore, 0)),
        "AttemptSemaphore",
    );
    assert_eq!(stolen.value, 0, "the lock belongs to the grantee");
}

#[test]
fn exec_shared_semaphores_admit_several_holders() {
    let k = harness::kernel();
    let a = harness::task(&k);
    let b = harness::task(&k);

    let semaphore = create(&k, a);

    // Two shared requests are both granted.
    let first = ok(
        harness::call(&k, a, numbers::OBTAIN_SEMAPHORE, args(semaphore, 1)),
        "ObtainSemaphore (shared)",
    );
    assert_eq!(first.value, 1);
    let second = ok(
        harness::call(&k, b, numbers::OBTAIN_SEMAPHORE, args(semaphore, 1)),
        "ObtainSemaphore (shared)",
    );
    assert_eq!(second.value, 1, "a shared lock admits a second holder");

    // An exclusive request still has to wait for both of them.
    let exclusive = ok(
        harness::call(&k, a, numbers::ATTEMPT_SEMAPHORE, args(semaphore, 0)),
        "AttemptSemaphore",
    );
    assert_eq!(exclusive.value, 0, "the shared holders keep it");
}

#[test]
fn exec_find_task_finds_a_named_task() {
    let k = harness::kernel();
    let t = harness::task(&k);
    let other = harness::task(&k);

    // A task names itself.
    ok(
        harness::call(&k, t, numbers::SET_TASK_NAME, args(cstring("Conductor"), 0)),
        "SetTaskName",
    );

    // `FindTask` finds it.
    let found = ok(
        harness::call(&k, other, numbers::FIND_TASK, args(cstring("Conductor"), 0)),
        "FindTask",
    );
    assert_eq!(found.value, t.0 as u64, "the named task is found");

    // A name nobody carries is an answer, not an error: AmigaOS returned
    // `NULL` there.
    let missing = ok(
        harness::call(&k, other, numbers::FIND_TASK, args(cstring("Nobody"), 0)),
        "FindTask",
    );
    assert_eq!(missing.value, 0, "an unknown name answers zero");

    // A null name asks for the calling task.
    let current = ok(
        harness::call(&k, other, numbers::FIND_TASK, SyscallArgs::default()),
        "FindTask (null)",
    );
    assert_eq!(current.value, other.0 as u64, "the caller finds itself");
}

#[test]
fn exec_alert_is_accepted_and_a_dead_alert_ends_the_task() {
    let k = harness::kernel();
    let t = harness::task(&k);

    // A recovery alert is logged and the task lives on.
    ok(
        harness::call(
            &k,
            t,
            numbers::ALERT,
            args(vela_abi_native64::alerts::exec::SEM_CORRUPT as u64, 0),
        ),
        "Alert",
    );
    assert!(
        k.exec().task_exists(t),
        "a recovery alert does not end the task"
    );

    // A dead alert ends the task — that is what `AT_DeadEnd` means.
    ok(
        harness::call(
            &k,
            t,
            numbers::ALERT,
            args(vela_abi_native64::alerts::exec::MEM_CORRUPT as u64, 0),
        ),
        "Alert (dead)",
    );
    assert!(
        !k.exec().task_exists(t),
        "a dead alert ends the calling task",
    );
}
