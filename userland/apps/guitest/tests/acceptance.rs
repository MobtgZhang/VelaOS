//! The M3/M4 acceptance criterion, driving the real `guitest` program.
//!
//! # What this file is
//!
//! `platforms/hosted/tests/idcmp_acceptance.rs` proves the *chain* works: that
//! a mouse event becomes an Exec message on a program's port. This file proves
//! something narrower and more valuable — that the **actual acceptance
//! application**, the one linked into the guest image, really does drive that
//! chain and really does report what it saw.
//!
//! The difference matters. The first file hand-writes the calls the report
//! describes. This one calls `vela_app::run()` and asserts on the outcome
//! it returns. A change to `guitest` that broke its window geometry, or that
//! stopped replying to messages, or that stopped closing its port, fails
//! *here* — and would not be caught by the chain test at all, because the chain
//! test never calls `guitest`.
//!
//! # How the program and the driver interleave
//!
//! `app::run()` is an ordinary function that blocks in `WaitPort`. A
//! blocking function cannot be called from a test that then needs to drive
//! events, so the program runs on its own thread and the test feeds it from
//! this one. The kernel services are internally synchronised, so this is a real
//! arrangement rather than a mock.
//!
//! # What it deliberately does not prove
//!
//! That the assembly gate reaches these services from ring 3. That is the boot
//! smoke test's job. Together they cover the path; neither alone does.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

use vela_hosted::TaskId;
use vela_kernel_input::{buttons, MouseEvent};
use vela_kernel_syscall::ids::WindowId;
use vela_kernel_syscall::numbers::{self as num, SyscallArgs};
use vela_sys::hosted;

use vela_guitest::{self as app, Failure, Outcome};

/// How many spins to wait for the program to open its window.
///
/// Bounded because the alternative is a hung test: the program and this
/// thread race, and if the program loses it will sit in `WaitPort` forever
/// with nothing to wake it. The count is generous — this is a thread
/// hand-off, not a timer — and giving up produces a failure message rather
/// than a hang.
const WINDOW_WAIT_SPINS: usize = 10_000_000;

/// How many spins to wait for the program to finish after the last event.
///
/// Bounded for the same reason, and because the program's own loop is bounded
/// too: a program-side hang must fail the test rather than stall the run.
const DRAIN_SPINS: usize = 10_000_000;

/// How many gadgets `guitest` puts on its window.
///
/// The driver waits for this many before clicking anything: a window exists a
/// good while before its gadgets do, and a click on a gadget-less window is
/// delivered correctly and then hits nothing.
const EXPECTED_GADGETS: usize = 2;

/// Serialises the drivers, because the tests share one desktop.
///
/// This is not a workaround for a data race — it is the model. There is one
/// pointer, one layer stack, and one "which window is in front". Two drivers
/// clicking at once is not a thing that can happen to a user, and letting the
/// tests do it produces failures that no real system could: driver A brings its
/// window forward, driver B brings its own, and A's remaining clicks land in
/// B's window. Serialising the *driving* keeps each test's window in front for
/// as long as that test is clicking, while the tests still share one kernel and
/// one window manager.
static DRIVER: std::sync::Mutex<()> = std::sync::Mutex::new(());

// ---------------------------------------------------------------------------
// The harness
// ---------------------------------------------------------------------------

/// The shared kernel the SDK's stubs are wired to.
///
/// `install` takes `&'static` services, so the kernel is leaked rather than
/// borrowed: a leaked kernel is a few kilobytes the test binary was going to
/// spend anyway, and a self-referential struct to express "the SDK needs
/// services that outlive it" would be worse.
///
/// # Why the task is *not* here
///
/// An earlier version cached one task and shared it across every test, on the
/// stated grounds that each test would get its own objects. It does not work:
/// the tests run in parallel against one Exec service, so two concurrent runs
/// share a task, and `drive_events` finds a window by title — meaning one
/// test's driver can click the *other* test's window, and a program can be sent
/// a message on a port it no longer owns. The symptom was `reply_msg` failing
/// with `NO_REPLY_PORT` in the program while the identical round trip passed in
/// isolation, which is the hardest kind of failure to read.
///
/// Tasks are cheap and the harness is a `OnceLock`, so the task is created per
/// run instead. The *kernel* stays shared: one Intuition service means the
/// tests still exercise the real window manager against each other, and the
/// per-test locks are the service's own.
fn harness() -> &'static vela_hosted::Kernel {
    static ONCE: std::sync::OnceLock<&'static vela_hosted::Kernel> = std::sync::OnceLock::new();
    ONCE.get_or_init(|| {
        let kernel: &'static vela_hosted::Kernel =
            Box::leak(Box::new(vela_hosted::Kernel::with_screen(1280, 800)));
        // SAFETY: `kernel` is leaked, so it outlives every program that uses
        // these services — which is exactly what `install` requires.
        unsafe {
            hosted::install(
                kernel.exec(),
                kernel.dos(),
                kernel.graphics(),
                kernel.intuition(),
                kernel.table(),
            )
        };
        kernel
    })
}

/// Run `guitest` while feeding it the mouse traffic it is waiting for.
///
/// Returns what the program saw, or the failure that stopped it.
fn run_guitest() -> Result<Outcome, Failure> {
    let kernel = harness();
    // A fresh task per run, so two tests running in parallel cannot end up
    // owning each other's ports. See `harness` for what that cost when they
    // did share one.
    let task = kernel.exec().create_task();
    kernel.set_current_task(task);
    // One pointer at a time, for the whole run — see `DRIVER`.
    let _driving = DRIVER
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    // `Arc` rather than a plain borrow: the worker thread outlives this scope
    // by exactly the length of `worker.join()` below, and the borrow checker
    // cannot see that through `thread::spawn`'s `'static` bound. Wrapping the
    // two pieces of shared state — rather than moving them in — is what lets
    // the driver keep using them after the program is done.
    //
    // `kernel` and `task` are copied out rather than captured as a reference to
    // `h`, because `Harness` itself must be `'static` to cross the boundary and
    // a reference to it would not be. Both fields are already `'static`.
    let result: Arc<Mutex<Option<Result<Outcome, Failure>>>> = Arc::new(Mutex::new(None));
    let done = Arc::new(AtomicBool::new(false));

    let worker = {
        let result = Arc::clone(&result);
        let done = Arc::clone(&done);
        thread::spawn(move || {
            // The program attributes every object it opens to the current task,
            // which on hardware the context switch arranges. Here it has to be
            // arranged explicitly — and by *this* thread, because the hosted
            // path reads "the current task" from the Exec service, which is
            // shared.
            kernel.set_current_task(task);
            *result.lock().unwrap() = Some(app::run());
            done.store(true, Ordering::SeqCst);
        })
    };

    drive_events(kernel, &done, task);
    worker.join().expect("guitest must not panic");

    // The lock is dropped before the value is returned. Holding a `MutexGuard`
    // across a `return` would tie the guard's lifetime to the result's, which
    // is the kind of borrow that compiles today and becomes a deadlock the day
    // someone adds an `?` in between.
    let outcome = {
        let mut slot = result.lock().unwrap();
        slot.take()
    };
    outcome.expect("the program reported")
}

/// Synthesise the mouse traffic the program is waiting for, until it finishes.
///
/// Every event is derived from the program's *own* published geometry, so the
/// test cannot pass by agreeing with a stale copy of it: if `guitest` moves its
/// window, this follows.
fn drive_events(kernel: &'static vela_hosted::Kernel, done: &AtomicBool, task: TaskId) {
    let h = kernel;
    // The program opens its window through the SDK, so find it by title rather
    // than by remembering a handle it has not told us.
    //
    // Waiting for it rather than checking once is the difference between a
    // harness and a race: the program runs on its own thread, so at this point
    // it may not have got as far as `OpenWindow` yet. A single check would
    // report "nothing to drive", return, and leave `worker.join()` waiting on a
    // program that is about to block in `WaitPort` forever.
    let window = wait_for_window(h, done, task);
    let Some(window) = window else {
        // The program finished without opening a window, or the drain ran out.
        // Either way it will report the failure itself.
        return;
    };

    let record = h.intuition().window(window).expect("the window exists");

    // Put this run's window in front before clicking anything.
    //
    // The tests share one Intuition service, so each run opens a screen and a
    // window, and the layer stack ends up with several overlapping layers. A
    // mouse event goes to the *topmost* layer under the pointer, which may be
    // another test's screen — and that screen has no gadget at these
    // coordinates, so the click falls through to a plain mouse movement and
    // the program under test never sees its own gadget.
    //
    // Bringing the window to front is what makes the driver and the program
    // agree about who has the pointer. It is also what a user does, so it is
    // not a test-only trick: on real hardware there is one desktop and one
    // pointer, and this is the arrangement that makes the test meaningful.
    let _ = h.intuition().layers().to_front(record.layer);

    // A bare movement over the button, first. It is the one event that changes
    // nothing, so a program that mishandled it shows up as a spurious count
    // rather than as a missing event — which is the harder failure to notice.
    let (bx, by) = app::button_screen_pos();
    let _ = h.intuition().dispatch_mouse(
        h.exec(),
        &MouseEvent {
            x: bx as i32,
            y: by as i32,
            buttons: 0,
            changed: 0,
        },
    );

    // A full press-and-release over the button.
    click(h, bx as i32, by as i32);

    // Then the toggle, so that routing by *position* is exercised rather than
    // assumed: a hit test that always returned the first gadget would pass a
    // one-gadget test.
    let (tx, ty) = app::toggle_screen_pos();
    click(h, tx as i32, ty as i32);

    // Then the window's close gadget, which is furniture rather than a
    // registered gadget and therefore arrives as an Intuition *request*. The
    // program — not the kernel — decides to act on it.
    if let Some(close) = record.close_gadget_rect() {
        click(
            h,
            close.x + close.width as i32 / 2,
            close.y + close.height as i32 / 2,
        );
    }

    // Give the program a bounded moment to drain what it was sent. Bounded
    // because the program's own loop is bounded too, and an unbounded wait
    // here would turn a program-side hang into a hung test run.
    for _ in 0..DRAIN_SPINS {
        if done.load(Ordering::SeqCst) {
            return;
        }
        thread::yield_now();
    }
}

/// Wait for *this run's* program to open its window.
///
/// Scoped to the owning task as well as the title. The title alone is not
/// enough: the tests share one Intuition service and run in parallel, so a
/// title-only search finds whichever `guitest` window happens to be at the
/// front — including another test's, whose messages go to a port this run does
/// not own. Scoping by owner makes the driver and the program agree about
/// which window they are talking about.
///
/// Bounded, and giving up as soon as the program finishes: a program that
/// exits without opening a window has failed, and waiting for a window that
/// will never appear would only turn a clean failure into a hung test.
fn wait_for_window(
    h: &'static vela_hosted::Kernel,
    done: &AtomicBool,
    task: TaskId,
) -> Option<WindowId> {
    for _ in 0..WINDOW_WAIT_SPINS {
        if done.load(Ordering::SeqCst) {
            return None;
        }
        if let Some(id) = h.intuition().windows_front_to_back().into_iter().find(|w| {
            h.intuition()
                .window(*w)
                .is_some_and(|record| record.title == "guitest" && record.owner == task)
        }) {
            // Wait for the gadgets, not just the window. `OpenWindow` and
            // `AddGadget` are separate calls, and a driver that clicks the
            // moment the window exists hits bare window interior: the click
            // routes correctly and then finds nothing there, so the program
            // sees a plain mouse movement and the gadget never fires. Waiting
            // for the window's own geometry — two gadgets, at the positions
            // the program publishes — is what makes the driver and the program
            // agree that the window is *ready*.
            let ready = h
                .intuition()
                .window(id)
                .is_some_and(|record| record.gadgets.len() >= EXPECTED_GADGETS);
            if ready {
                return Some(id);
            }
        }
        thread::yield_now();
    }
    None
}

/// Press and release the left button at a point.
fn click(k: &'static vela_hosted::Kernel, x: i32, y: i32) {
    let _ = k.intuition().dispatch_mouse(
        k.exec(),
        &MouseEvent {
            x,
            y,
            buttons: buttons::LEFT,
            changed: buttons::LEFT,
        },
    );
    let _ = k.intuition().dispatch_mouse(
        k.exec(),
        &MouseEvent {
            x,
            y,
            buttons: 0,
            changed: buttons::LEFT,
        },
    );
}

// ---------------------------------------------------------------------------
// The tests
// ---------------------------------------------------------------------------

#[test]
fn the_acceptance_program_completes_the_whole_chain() {
    let outcome = run_guitest().expect("guitest must complete its chain");

    // The specific numbers, not "something arrived": the report's criterion is
    // that a window opens, a mouse event is received *as a message*, it is
    // replied to, and the window closes. Each of those is its own assertion,
    // and each would pass on a chain that only half works.
    assert!(
        outcome.gadget_events >= 2,
        "the program must receive a gadget event per press, got {outcome:?}",
    );
    assert!(
        outcome.button_downs >= 2,
        "both presses must arrive with the button down, got {outcome:?}",
    );
    assert!(
        outcome.button_ups >= 2,
        "both releases must arrive with the button up, got {outcome:?}",
    );
    assert!(
        outcome.close_events >= 1,
        "the close gadget must deliver a close request, got {outcome:?}",
    );
    assert_eq!(
        outcome.replies, outcome.requests,
        "every request must be replied to: a request left unreplied is a \
         sender waiting forever",
    );
    assert!(
        outcome.requests > 0,
        "the program must have received something worth replying to, got \
         {outcome:?}",
    );
    assert!(
        outcome.is_accepted(),
        "the program's own acceptance check must agree with the test's, got {outcome:?}",
    );
}

#[test]
fn the_program_closes_the_window_it_opened() {
    let kernel = harness();
    let before = kernel.intuition().window_count();

    let outcome = run_guitest().expect("guitest must complete its chain");
    assert!(outcome.close_events >= 1, "the window must be closed");

    // The window count goes back to what it was. Not "to zero": the file's
    // other tests share the kernel and run in parallel, so asserting an
    // absolute count would make this test depend on how the runner scheduled
    // it.
    let after = kernel.intuition().window_count();
    assert_eq!(after, before, "the program must leave no window behind",);
}

#[test]
fn a_program_that_passes_a_bad_pointer_gets_an_error_not_a_fault() {
    // The plan requires this case explicitly, and it belongs here rather than
    // only in the conformance suite because this is where a *program* is
    // driving it — a program is the thing most likely to get a pointer wrong.
    let kernel = harness();
    // Its own task, for the same reason `run_guitest` makes one: a shared task
    // would let a concurrently running test's program own this call's objects.
    let task = kernel.exec().create_task();
    let result = kernel.syscall(
        task,
        num::PUT_MSG,
        SyscallArgs::new(
            // A port that exists, and a message pointer in the kernel half.
            // The kernel must refuse it rather than dereference it.
            u64::MAX,
            0xFFFF_8000_0000_0000,
            0,
            0,
            0,
            0,
        ),
    );
    assert_eq!(
        result.error,
        vela_sys::errors::BAD_POINTER,
        "a kernel-half pointer must be refused, not followed",
    );
}
