//! The `exec.library` semaphores and task names, driven through the SDK.
//!
//! The tests in `exec_semaphore_conformance.rs` drive the kernel's syscall
//! table directly. This file drives the *other* half: the SDK's typed
//! wrappers, the library base, the generated jump table and the stub.
//!
//! # Why the second half needs its own tests
//!
//! The SDK's own documentation warns about the failure this file exists to
//! catch: a wrapper whose argument order disagrees with the kernel's handler
//! compiles, passes the SDK's unit tests, and produces nonsense on
//! hardware. The jump table's offsets are generated from the IDL, and the
//! argument order is written by hand on both sides — so the only thing that
//! proves they agree is a test that goes through the whole chain.
//!
//! Every call below therefore starts at `vela_sys::api::…`, crosses the
//! library base and the generated table, and lands in the kernel's handler.

use std::sync::{Mutex, MutexGuard, OnceLock};

use vela_sys::api;
use vela_sys::hosted;

use vela_test_api_conformance::harness;

/// Guards the SDK's process-global hosted services.
fn serial() -> MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// The one kernel installed for the SDK-driven tests in this file.
fn services() -> &'static vela_hosted::Kernel {
    static SERVICES: OnceLock<&'static vela_hosted::Kernel> = OnceLock::new();
    SERVICES.get_or_init(|| {
        let kernel: &'static vela_hosted::Kernel = Box::leak(Box::new(harness::kernel()));
        // SAFETY: the kernel is leaked, so every reference handed to
        // `install` outlives the process.
        unsafe {
            hosted::install(
                kernel.exec(),
                kernel.dos(),
                kernel.graphics(),
                kernel.intuition(),
                kernel.icon(),
                kernel.workbench(),
                kernel.table(),
            );
        }
        kernel
    })
}

/// Install the SDK services and name a task for the current thread.
fn ready() -> MutexGuard<'static, ()> {
    let guard = serial();
    let kernel = services();
    let task = harness::task(kernel);
    kernel.exec().set_current_task(task);
    guard
}

#[test]
fn sdk_find_task_and_set_task_name() {
    let _guard = ready();

    api::set_task_name("Conductor").expect("SetTaskName");

    // A named task is found by name.
    let found = api::find_task(Some("Conductor")).expect("FindTask");
    assert_ne!(found, 0, "the named task must be found");

    // An unknown name answers zero rather than an error.
    let missing = api::find_task(Some("Nobody")).expect("FindTask");
    assert_eq!(missing, 0, "an unknown name answers zero");

    // No name asks for the calling task.
    let current = api::find_task(None).expect("FindTask");
    assert_ne!(current, 0, "the caller finds itself");
}

#[test]
fn sdk_semaphore_round_trip() {
    let _guard = ready();

    let semaphore = api::create_semaphore(0).expect("InitSemaphore");
    assert!(!semaphore.is_null(), "a semaphore must have an id");

    // A free lock is granted at once.
    assert!(
        api::obtain_semaphore(semaphore, false).expect("ObtainSemaphore"),
        "the lock is taken"
    );

    // The owner nests, and each level needs its own release.
    assert!(
        api::obtain_semaphore(semaphore, false).expect("ObtainSemaphore"),
        "the owner nests"
    );
    api::release_semaphore(semaphore).expect("ReleaseSemaphore");
    api::release_semaphore(semaphore).expect("ReleaseSemaphore");

    // A third release is the corruption AROS alerted about.
    assert!(
        api::release_semaphore(semaphore).is_err(),
        "a release the caller does not own must be refused"
    );

    // The attempt answers "not now" rather than blocking or failing.
    let other = api::obtain_semaphore(semaphore, false).expect("ObtainSemaphore");
    assert!(other, "the lock is free after the releases");

    api::delete_semaphore(semaphore).expect("DeleteSemaphore");
    assert!(
        api::attempt_semaphore(semaphore, false).is_err(),
        "a deleted semaphore says so"
    );
}

#[test]
fn sdk_shared_semaphore_admits_two_holders() {
    let _guard = ready();

    let semaphore = api::create_semaphore(0).expect("InitSemaphore");
    assert!(
        api::obtain_semaphore(semaphore, true).expect("ObtainSemaphore (shared)"),
        "the first shared holder is granted"
    );
    assert!(
        api::obtain_semaphore(semaphore, true).expect("ObtainSemaphore (shared)"),
        "the second shared holder is granted"
    );
    // An exclusive request still has to wait for both of them.
    assert!(
        !api::attempt_semaphore(semaphore, false).expect("AttemptSemaphore"),
        "the shared holders keep the lock"
    );
    api::delete_semaphore(semaphore).expect("DeleteSemaphore");
}

#[test]
fn sdk_named_semaphore_is_found_by_name() {
    let _guard = ready();

    let semaphore = api::add_semaphore("Conductor", 0).expect("AddSemaphore");
    assert!(!semaphore.is_null());

    // The public list is system-wide: the same name resolves to the same
    // semaphore.
    let found = api::find_semaphore("Conductor").expect("FindSemaphore");
    assert_eq!(found, semaphore, "the named semaphore is found");

    // An unknown name answers null rather than an error.
    let missing = api::find_semaphore("Nobody").expect("FindSemaphore");
    assert!(missing.is_null());

    // Unlisting it leaves the semaphore alive but no longer findable.
    let removed = api::remove_semaphore("Conductor").expect("RemoveSemaphore");
    assert_eq!(removed, semaphore);
    assert!(api::find_semaphore("Conductor")
        .expect("FindSemaphore")
        .is_null());

    // And it still works by id.
    assert!(api::obtain_semaphore(semaphore, false).expect("ObtainSemaphore"));
    api::delete_semaphore(semaphore).expect("DeleteSemaphore");
}

#[test]
fn sdk_alert_is_accepted() {
    let _guard = ready();

    // A recovery alert is logged and the program carries on.
    api::alert(vela_sys::api::alerts::exec::SEM_CORRUPT).expect("Alert");
}
