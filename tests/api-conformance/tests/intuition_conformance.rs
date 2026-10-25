//! The `intuition.library` conformance behaviour tests.
//!
//! Intuition owns screens, windows and gadgets. The behaviour worth pinning
//! down is the *ownership* rules: a window belongs to the task that opened
//! it, a window's message port belongs to the program rather than to the
//! window, and the geometry a program passes is validated rather than
//! trusted.

use vela_kernel_intuition::errors as intuition_errors;
use vela_kernel_syscall::numbers::{self, errors, SyscallArgs};

use vela_test_api_conformance::harness::{self, ok};

/// The geometry a program passes to `OpenWindow`.
///
/// The layout is the one `kernel/syscall`'s `OpenWindowArgs` documents: two
/// `i16` coordinates, two `u16` dimensions, an `u16` IDCMP mask and a `u32`
/// flags word.
#[repr(C)]
#[derive(Clone, Copy)]
struct OpenWindowArgs {
    /// Left edge.
    x: i16,
    /// Top edge.
    y: i16,
    /// Width.
    width: u16,
    /// Height.
    height: u16,
    /// The IDCMP class mask.
    idcmp: u16,
    /// The window flags.
    flags: u32,
}

/// A 200 by 100 window at the origin, with no events requested.
fn window_args() -> OpenWindowArgs {
    OpenWindowArgs {
        x: 0,
        y: 0,
        width: 200,
        height: 100,
        idcmp: 0,
        flags: 0,
    }
}

/// Open a screen for the tests.
fn open_screen(k: &vela_hosted::Kernel, t: vela_kernel_exec::task::TaskId) -> u64 {
    ok(
        harness::call(
            k,
            t,
            numbers::INTUITION_OPEN_SCREEN,
            SyscallArgs::new(harness::cstring("test"), 0, 0, 640, 480, 0),
        ),
        "OpenScreen",
    )
    .value
}

/// `OpenScreen`, `OpenWindow` and `CloseWindow`: the lifecycle, and the
/// ownership rule that goes with it.
///
/// A zero-sized screen is refused with `BAD_RECT`, and closing a window
/// that belongs to another task is `NOT_OWNER` — a program that closed
/// someone else's window has a bug, and hiding it produces two programs
/// fighting over one window.
#[test]
fn intuition_screen_and_window_lifecycle() {
    let k = harness::kernel();
    let t = harness::task(&k);

    // A screen with no size is refused.
    let empty = harness::call(
        &k,
        t,
        numbers::INTUITION_OPEN_SCREEN,
        SyscallArgs::new(harness::cstring("empty"), 0, 0, 0, 0, 0),
    );
    assert!(empty.is_err(), "a zero-sized screen must be refused");
    assert_eq!(empty.error, intuition_errors::BAD_RECT);

    let screen = open_screen(&k, t);
    assert_ne!(screen, 0, "a screen handle must not be zero");

    let window = ok(
        harness::call(
            &k,
            t,
            numbers::INTUITION_OPEN_WINDOW,
            SyscallArgs::new(
                screen,
                harness::cstring("window"),
                harness::struct_bytes(&window_args()),
                0,
                0,
                0,
            ),
        ),
        "OpenWindow",
    )
    .value;
    assert_ne!(window, 0, "a window handle must not be zero");

    // Another task may not close it.
    let other = harness::task(&k);
    let stolen = harness::call(
        &k,
        other,
        numbers::INTUITION_CLOSE_WINDOW,
        SyscallArgs::new(window, 0, 0, 0, 0, 0),
    );
    assert!(stolen.is_err(), "closing another task's window must fail");
    assert_eq!(stolen.error, errors::NOT_OWNER);

    // The owner can.
    ok(
        harness::call(
            &k,
            t,
            numbers::INTUITION_CLOSE_WINDOW,
            SyscallArgs::new(window, 0, 0, 0, 0, 0),
        ),
        "CloseWindow",
    );

    // Closing a window that does not exist reports `NO_OBJECT`.
    let gone = harness::call(
        &k,
        t,
        numbers::INTUITION_CLOSE_WINDOW,
        SyscallArgs::new(window, 0, 0, 0, 0, 0),
    );
    assert!(gone.is_err(), "closing a dead window must fail");
    assert_eq!(gone.error, errors::NO_OBJECT);
}

/// `OpenWindow` on a screen that does not exist is refused.
#[test]
fn intuition_open_window_on_an_unknown_screen_is_refused() {
    let k = harness::kernel();
    let t = harness::task(&k);

    let result = harness::call(
        &k,
        t,
        numbers::INTUITION_OPEN_WINDOW,
        SyscallArgs::new(
            4242,
            harness::cstring("window"),
            harness::struct_bytes(&window_args()),
            0,
            0,
            0,
        ),
    );
    assert!(result.is_err(), "an unknown screen must be refused");
    assert_eq!(result.error, errors::NO_OBJECT);
}

/// `OpenWindow` whose geometry arrives behind a bad pointer is refused.
///
/// The geometry is behind a pointer so that it can be validated; a pointer
/// in the kernel half is refused without being read.
#[test]
fn intuition_open_window_needs_a_valid_geometry_pointer() {
    let k = harness::kernel();
    let t = harness::task(&k);
    let screen = open_screen(&k, t);

    let result = harness::call(
        &k,
        t,
        numbers::INTUITION_OPEN_WINDOW,
        SyscallArgs::new(
            screen,
            harness::cstring("window"),
            harness::KERNEL_ADDRESS,
            0,
            0,
            0,
        ),
    );
    assert!(result.is_err(), "a kernel pointer must be refused");
    assert_eq!(result.error, errors::BAD_POINTER);
}

/// `SetTitle`, `SetAttrs` and `AddGadget`: the window's furniture.
///
/// The title is copied out of user space before it is stored, so a program
/// that frees its buffer afterwards does not leave the kernel holding a
/// pointer into it.
#[test]
fn intuition_window_title_and_gadgets() {
    let k = harness::kernel();
    let t = harness::task(&k);
    let screen = open_screen(&k, t);

    let window = ok(
        harness::call(
            &k,
            t,
            numbers::INTUITION_OPEN_WINDOW,
            SyscallArgs::new(
                screen,
                harness::cstring("first"),
                harness::struct_bytes(&window_args()),
                0,
                0,
                0,
            ),
        ),
        "OpenWindow",
    )
    .value;

    // The title is the one the program asked for.
    assert_eq!(
        k.intuition()
            .window(vela_kernel_intuition::WindowId(window as u32))
            .map(|w| w.title)
            .as_deref(),
        Some("first"),
    );

    // A new title replaces it.
    ok(
        harness::call(
            &k,
            t,
            numbers::INTUITION_SET_TITLE,
            SyscallArgs::new(window, harness::cstring("second"), 0, 0, 0, 0),
        ),
        "SetTitle",
    );
    assert_eq!(
        k.intuition()
            .window(vela_kernel_intuition::WindowId(window as u32))
            .map(|w| w.title)
            .as_deref(),
        Some("second"),
    );

    // A gadget with a rectangle and a label.
    let rect = vela_kernel_syscall::copy::UserRect::new(4, 4, 32, 16);
    let gadget = ok(
        harness::call(
            &k,
            t,
            numbers::INTUITION_ADD_GADGET,
            SyscallArgs::new(
                window,
                0,
                harness::struct_bytes(&rect),
                harness::cstring("OK"),
                0,
                0,
            ),
        ),
        "AddGadget",
    );
    assert_ne!(gadget.value, 0, "a gadget handle must not be zero");

    let record = k
        .intuition()
        .gadget(vela_kernel_intuition::GadgetId(gadget.value as u32))
        .expect("the gadget must exist");
    assert_eq!(record.label, "OK");
    assert_eq!(record.bounds.width, 32);

    // The window's flags and IDCMP mask are the program's to set.
    ok(
        harness::call(
            &k,
            t,
            numbers::INTUITION_SET_ATTRS,
            SyscallArgs::new(window, 0, 0, 0, 0, 0),
        ),
        "SetAttrs",
    );
    let window_record = k
        .intuition()
        .window(vela_kernel_intuition::WindowId(window as u32))
        .expect("the window must exist");
    assert_eq!(window_record.idcmp, 0);
}

/// A window that asks for events but has no port is refused.
///
/// Silently dropping the events would make the program hang waiting for
/// something that can never arrive.
#[test]
fn intuition_a_window_asking_for_events_needs_a_port() {
    let k = harness::kernel();
    let t = harness::task(&k);
    let screen = open_screen(&k, t);

    let args = OpenWindowArgs {
        x: 0,
        y: 0,
        width: 100,
        height: 100,
        idcmp: vela_kernel_intuition::idcmp::MOUSEMOVE,
        flags: 0,
    };

    let result = harness::call(
        &k,
        t,
        numbers::INTUITION_OPEN_WINDOW,
        SyscallArgs::new(
            screen,
            harness::cstring("window"),
            harness::struct_bytes(&args),
            0,
            0,
            0,
        ),
    );
    assert!(
        result.is_err(),
        "a window that wants events must have a port",
    );
    assert_eq!(result.error, intuition_errors::NO_PORT);
}

/// `CloseScreen` closes the windows that were on it, and says how many.
///
/// A window left open on a screen that is gone is a handle that names
/// nothing, so the screen takes its windows with it.
#[test]
fn intuition_close_screen_closes_its_windows() {
    let k = harness::kernel();
    let t = harness::task(&k);
    let screen = open_screen(&k, t);

    let window = ok(
        harness::call(
            &k,
            t,
            numbers::INTUITION_OPEN_WINDOW,
            SyscallArgs::new(
                screen,
                harness::cstring("window"),
                harness::struct_bytes(&window_args()),
                0,
                0,
                0,
            ),
        ),
        "OpenWindow",
    )
    .value;

    let closed = ok(
        harness::call(
            &k,
            t,
            numbers::INTUITION_CLOSE_SCREEN,
            SyscallArgs::new(screen, 0, 0, 0, 0, 0),
        ),
        "CloseScreen",
    );
    assert_eq!(closed.value, 1, "closing a screen must close its windows");
    assert_eq!(k.intuition().window_count(), 0);

    // The window handle names nothing now.
    let gone = harness::call(
        &k,
        t,
        numbers::INTUITION_CLOSE_WINDOW,
        SyscallArgs::new(window, 0, 0, 0, 0, 0),
    );
    assert!(gone.is_err());
}
