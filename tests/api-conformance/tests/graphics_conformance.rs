//! The `graphics.library` conformance behaviour tests.
//!
//! A pen is an index into a palette, and the colour it resolves to is what
//! a program draws with. The interesting behaviour is the *ownership* of a
//! pen and the *validation* of the geometry a drawing call brings with it.

use vela_kernel_graphics::errors as graphics_errors;
use vela_kernel_syscall::numbers::{self, errors, SyscallArgs};

use vela_test_api_conformance::harness::{self, ok};

/// The colour the tests draw with: opaque blue.
const BLUE: u32 = 0x0000_00FF;

/// `AllocPen`, `SetPen` and `FreePen`: a pen is allocated, resolves to its
/// colour, and is released exactly once.
///
/// Pen zero is a legitimate pen, so the failure value is `0xFFFF` rather
/// than zero: a program that could not tell the two apart would silently
/// draw with the wrong colour.
#[test]
fn graphics_pen_lifecycle() {
    let k = harness::kernel();
    let t = harness::task(&k);

    let pen = ok(
        harness::call(
            &k,
            t,
            numbers::GRAPHICS_ALLOC_PEN,
            SyscallArgs::new(0, BLUE as u64, 0, 0, 0, 0),
        ),
        "AllocPen",
    );
    assert_ne!(pen.value, 0xFFFF, "the first pen allocated is pen zero");

    // The pen resolves to the colour it was allocated with.
    let color = ok(
        harness::call(
            &k,
            t,
            numbers::GRAPHICS_SET_PEN,
            SyscallArgs::new(0, pen.value, 0, 0, 0, 0),
        ),
        "SetPen",
    );
    assert_eq!(color.value, BLUE as u64);

    ok(
        harness::call(
            &k,
            t,
            numbers::GRAPHICS_FREE_PEN,
            SyscallArgs::new(0, pen.value, 0, 0, 0, 0),
        ),
        "FreePen",
    );

    // Releasing a pen twice reports `NO_OBJECT`: the pen is gone.
    let twice = harness::call(
        &k,
        t,
        numbers::GRAPHICS_FREE_PEN,
        SyscallArgs::new(0, pen.value, 0, 0, 0, 0),
    );
    assert!(twice.is_err(), "releasing a pen twice must fail");
    assert_eq!(twice.error, graphics_errors::NO_OBJECT);
}

/// `AllocPen` on a palette that does not exist is refused.
#[test]
fn graphics_alloc_pen_on_an_unknown_palette_is_refused() {
    let k = harness::kernel();
    let t = harness::task(&k);

    let result = harness::call(
        &k,
        t,
        numbers::GRAPHICS_ALLOC_PEN,
        SyscallArgs::new(999, BLUE as u64, 0, 0, 0, 0),
    );
    assert!(result.is_err(), "an unknown palette must be refused");
    assert_eq!(result.error, graphics_errors::BAD_PALETTE);
}

/// `SetPen` for a pen that was never allocated reports `NO_OBJECT`.
#[test]
fn graphics_set_pen_of_an_unknown_pen_is_refused() {
    let k = harness::kernel();
    let t = harness::task(&k);

    let result = harness::call(
        &k,
        t,
        numbers::GRAPHICS_SET_PEN,
        SyscallArgs::new(0, 4242, 0, 0, 0, 0),
    );
    assert!(result.is_err(), "an unknown pen must be refused");
    assert_eq!(result.error, errors::NO_OBJECT);
}

/// The rectangle behind a drawing call is validated, not trusted.
///
/// `Rect` and `FillRect` take their geometry behind a pointer so that the
/// kernel can validate it. A pointer in the kernel half is refused without
/// being read at all, and an unmapped user pointer is refused by the probe.
/// Neither is a fault: both are ordinary errors.
#[test]
fn graphics_rect_needs_a_valid_pointer() {
    let k = harness::kernel();
    let t = harness::task(&k);

    // A window to draw into, so the pointer is the only thing under test.
    let screen = ok(
        harness::call(
            &k,
            t,
            numbers::INTUITION_OPEN_SCREEN,
            SyscallArgs::new(harness::cstring("test"), 0, 0, 640, 480, 0),
        ),
        "OpenScreen",
    )
    .value;
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

    let pen = ok(
        harness::call(
            &k,
            t,
            numbers::GRAPHICS_ALLOC_PEN,
            SyscallArgs::new(0, BLUE as u64, 0, 0, 0, 0),
        ),
        "AllocPen",
    )
    .value;

    // A pointer in the kernel half: refused before it is read.
    let kernel_pointer = harness::call(
        &k,
        t,
        numbers::GRAPHICS_FILL_RECT,
        SyscallArgs::new(window, pen, harness::KERNEL_ADDRESS, 0, 0, 0),
    );
    assert!(kernel_pointer.is_err(), "a kernel pointer must be refused");
    assert_eq!(kernel_pointer.error, errors::BAD_POINTER);

    // A user pointer that is not mapped: refused by the probe.
    let unmapped = harness::call(
        &k,
        t,
        numbers::GRAPHICS_FILL_RECT,
        SyscallArgs::new(window, pen, harness::UNMAPPED_USER_ADDRESS, 0, 0, 0),
    );
    assert!(unmapped.is_err(), "an unmapped pointer must be refused");
    assert_eq!(unmapped.error, errors::BAD_POINTER);

    // A good pointer works, so the refusals above are about the pointer and
    // not about the call being broken.
    let rect = vela_test_api_conformance::ByteBuffer::with(&rect_bytes());
    let good = harness::call(
        &k,
        t,
        numbers::GRAPHICS_FILL_RECT,
        SyscallArgs::new(window, pen, rect.addr(), 0, 0, 0),
    );
    assert!(good.is_ok(), "a valid rectangle must be accepted");
}

/// `Text` draws into the window and reports the advance width.
///
/// The width is the number of glyphs times the font's glyph width, so a
/// program can lay text out without asking the kernel for the font's metrics.
#[test]
fn graphics_text_draws_into_the_window() {
    let k = harness::kernel();
    let t = harness::task(&k);

    let screen = ok(
        harness::call(
            &k,
            t,
            numbers::INTUITION_OPEN_SCREEN,
            SyscallArgs::new(harness::cstring("test"), 0, 0, 640, 480, 0),
        ),
        "OpenScreen",
    )
    .value;
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

    let pen = ok(
        harness::call(
            &k,
            t,
            numbers::GRAPHICS_ALLOC_PEN,
            SyscallArgs::new(0, BLUE as u64, 0, 0, 0, 0),
        ),
        "AllocPen",
    )
    .value;

    let text = "abc";
    let width = ok(
        harness::call(
            &k,
            t,
            numbers::GRAPHICS_TEXT,
            SyscallArgs::new(window, pen, 4, 4, harness::cstring(text), 0),
        ),
        "Text",
    );
    // The font's glyph width is eight pixels.
    assert_eq!(width.value, 3 * 8);

    // The pixels landed in the window's layer.
    let layer = k
        .intuition()
        .window(vela_kernel_intuition::WindowId(window as u32))
        .map(|w| w.layer)
        .expect("the window must exist");
    let painted = k
        .intuition()
        .layers()
        .with_layer(layer, |l| {
            (0..16)
                .flat_map(|row| (0..24).map(move |col| (col, row)))
                .filter_map(|(col, row)| l.get(col, row))
                .any(|pixel| pixel != 0)
        })
        .expect("the layer must exist");
    assert!(
        painted,
        "the text must have drawn something into the window"
    );
}

/// The rectangle the tests draw: a 10 by 10 box at the origin.
fn rect_bytes() -> [u8; 8] {
    let rect = vela_kernel_syscall::copy::UserRect::new(0, 0, 10, 10);
    let mut out = [0u8; 8];
    // SAFETY: `rect` is a live local, so the pointer is valid for the eight
    // bytes that are read. `UserRect` is `#[repr(C)]` and is exactly eight
    // bytes, and `out` is eight bytes long, so the copy cannot go out of
    // bounds on either side.
    out.copy_from_slice(unsafe {
        std::slice::from_raw_parts(
            &rect as *const vela_kernel_syscall::copy::UserRect as *const u8,
            8,
        )
    });
    out
}

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

/// The window geometry the tests use: a 200 by 100 window at the origin.
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
