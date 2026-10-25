//! The Intuition-library syscall handlers.
//!
//! # The message port is the program's, not the window's
//!
//! `OpenWindow` takes a `port` argument: an Exec port the program already
//! created. Intuition does not create a port for the window and it does not
//! keep a reference of its own, because a port owned by the window would have
//! to be deleted when the window closed — and a program that opened two
//! windows on one port (a legitimate thing to do) would lose the second
//! window's port when the first closed.
//!
//! So the lifetime is the program's, and this is what makes the report's
//! acceptance path work unchanged: the program calls `CreateMsgPort`, passes
//! the handle to `OpenWindow`, and receives IDCMP messages through
//! `WaitPort`/`GetMsg` on the port it owns.
//!
//! # Ownership of windows
//!
//! A window belongs to the task that opened it. `CloseWindow` on someone
//! else's window is `NOT_OWNER`, not a silent success — a program that closes
//! a window it does not own has a bug, and hiding it produces two programs
//! fighting over one window.

use vela_kernel_exec::port::PortId;
use vela_kernel_intuition::{idcmp, window_flags, WindowId};

use crate::copy::read_user_rect;
use crate::dispatch::SyscallContext;
use crate::numbers::{errors, SyscallArgs, SyscallResult};

/// The `WAGA`/`WINDOW` geometry a program passes to `OpenWindow`.
///
/// Fixed-size rather than a pointer to a `struct Window`, because a pointer
/// would make the kernel read fields it does not need and give the program a
/// way to pass an uninitialised structure.
#[derive(Debug, Clone, Copy)]
#[repr(C)]
pub struct OpenWindowArgs {
    /// Left edge, in screen coordinates.
    pub x: i16,
    /// Top edge.
    pub y: i16,
    /// Width.
    pub width: u16,
    /// Height.
    pub height: u16,
    /// The IDCMP class mask.
    pub idcmp: u16,
    /// Window flags, from `vela_kernel_intuition::window_flags`.
    pub flags: u32,
}

/// `OpenScreen` — open a screen.
///
/// Arguments: `title_ptr` (a user NUL-terminated string), `x`, `y`, `width`,
/// `height`, `palette`. Returns the screen handle, or zero on failure.
pub fn open_screen(args: SyscallArgs, ctx: &SyscallContext<'_>) -> SyscallResult {
    let title = match crate::copy::copy_string_alloc(args.get(0)) {
        Ok(s) => s,
        Err(e) => return SyscallResult::err(e),
    };
    let bounds = vela_renderer2d::Rect::new(
        args.get(1) as i32 as i16 as i32,
        args.get(2) as i32 as i16 as i32,
        args.get(3) as u16 as u32,
        args.get(4) as u16 as u32,
    );
    match ctx
        .intuition
        .open_screen(&title, bounds, args.get(5) as u16)
    {
        Ok(id) => SyscallResult::ok(id.0 as u64),
        Err(e) => SyscallResult::err(e),
    }
}

/// `CloseScreen` — close a screen and every window on it.
///
/// Arguments: `screen`. Returns how many windows were closed as a side
/// effect, so the program can tell that its windows are gone rather than
/// discovering it one failed `CloseWindow` at a time.
pub fn close_screen(args: SyscallArgs, ctx: &SyscallContext<'_>) -> SyscallResult {
    let screen = vela_kernel_intuition::ScreenId(args.get(0) as u32);
    match ctx.intuition.close_screen(ctx.exec, screen) {
        Ok(closed) => SyscallResult::ok(closed as u64),
        Err(e) => SyscallResult::err(e),
    }
}

/// `OpenWindow` — open a window.
///
/// Arguments: `screen`, `title_ptr`, `args_ptr` (a user pointer to an
/// [`OpenWindowArgs`]), `port`. Returns the window handle, or zero on failure.
pub fn open_window(args: SyscallArgs, ctx: &SyscallContext<'_>) -> SyscallResult {
    let screen = vela_kernel_intuition::ScreenId(args.get(0) as u32);
    let title = match crate::copy::copy_string_alloc(args.get(1)) {
        Ok(s) => s,
        Err(e) => return SyscallResult::err(e),
    };
    let mut spec = OpenWindowArgs {
        x: 0,
        y: 0,
        width: 0,
        height: 0,
        idcmp: 0,
        flags: 0,
    };
    // SAFETY: the destination is a local, and the source is a user pointer
    // that `copy_from_user` validates and probes.
    if unsafe {
        crate::copy::copy_from_user(
            &mut spec as *mut OpenWindowArgs as *mut u8,
            args.get(2),
            core::mem::size_of::<OpenWindowArgs>(),
        )
    }
    .is_err()
    {
        return SyscallResult::err(errors::BAD_POINTER);
    }
    let bounds = vela_renderer2d::Rect::new(
        spec.x as i32,
        spec.y as i32,
        spec.width as u32,
        spec.height as u32,
    );
    let port = PortId(args.get(3) as u32);
    match ctx.intuition.open_window(
        screen, &title, bounds, spec.flags, spec.idcmp, port, ctx.task,
    ) {
        Ok(id) => SyscallResult::ok(id.0 as u64),
        Err(e) => SyscallResult::err(e),
    }
}

/// `CloseWindow` — close a window.
///
/// Arguments: `window`.
pub fn close_window(args: SyscallArgs, ctx: &SyscallContext<'_>) -> SyscallResult {
    let window = WindowId(args.get(0) as u32);
    match check_owner(ctx, window) {
        Ok(()) => {}
        Err(e) => return SyscallResult::err(e),
    }
    match ctx.intuition.close_window(ctx.exec, window) {
        Ok(()) => SyscallResult::ok(0),
        Err(e) => SyscallResult::err(e),
    }
}

/// `SetWindowTitle` — a window's title.
///
/// Arguments: `window`, `title_ptr`.
pub fn set_title(args: SyscallArgs, ctx: &SyscallContext<'_>) -> SyscallResult {
    let window = WindowId(args.get(0) as u32);
    if let Err(e) = check_owner(ctx, window) {
        return SyscallResult::err(e);
    }
    let title = match crate::copy::copy_string_alloc(args.get(1)) {
        Ok(s) => s,
        Err(e) => return SyscallResult::err(e),
    };
    match ctx.intuition.set_title(window, &title) {
        Ok(()) => SyscallResult::ok(0),
        Err(e) => SyscallResult::err(e),
    }
}

/// `SetWindowAttrs` — a window's flags and IDCMP mask.
///
/// Arguments: `window`, `flags`, `idcmp`.
pub fn set_attrs(args: SyscallArgs, ctx: &SyscallContext<'_>) -> SyscallResult {
    let window = WindowId(args.get(0) as u32);
    if let Err(e) = check_owner(ctx, window) {
        return SyscallResult::err(e);
    }
    let flags = args.get(1) as u32;
    let idcmp = args.get(2) as u16;
    match ctx.intuition.set_attrs(window, flags, idcmp) {
        Ok(()) => SyscallResult::ok(0),
        Err(e) => SyscallResult::err(e),
    }
}

/// `MoveWindow` / `SizeWindow` — a window's geometry.
///
/// Arguments: `window`, `x`, `y`, `width`, `height`. A zero width or height
/// means "leave that dimension alone", which is what makes one call serve both
/// `MoveWindow` and `SizeWindow` without either needing to read the current
/// geometry first.
pub fn set_bounds(args: SyscallArgs, ctx: &SyscallContext<'_>) -> SyscallResult {
    let window = WindowId(args.get(0) as u32);
    if let Err(e) = check_owner(ctx, window) {
        return SyscallResult::err(e);
    }
    let x = args.get(1) as i32 as i16 as i32;
    let y = args.get(2) as i32 as i16 as i32;
    let w = args.get(3) as u16 as u32;
    let h = args.get(4) as u16 as u32;
    let current = match ctx.intuition.window(window) {
        Some(w) => w.bounds,
        None => return SyscallResult::err(errors::NO_OBJECT),
    };
    let bounds = vela_renderer2d::Rect::new(
        x,
        y,
        if w == 0 { current.width } else { w },
        if h == 0 { current.height } else { h },
    );
    match ctx.intuition.set_window_bounds(window, bounds) {
        Ok(old) => SyscallResult::ok(pack_rect(old)),
        Err(e) => SyscallResult::err(e),
    }
}

/// `RefreshWindow` — repaint a window and composite.
///
/// Arguments: `window`, `flags` (non-zero also composites).
///
/// Repainting is separated from compositing because the two cost very
/// different amounts: a repaint redraws the window's own furniture, while a
/// composite copies it to the screen. A program that redraws in a loop wants
/// the first far more often than the second.
pub fn repaint(args: SyscallArgs, ctx: &SyscallContext<'_>) -> SyscallResult {
    let window = WindowId(args.get(0) as u32);
    if let Err(e) = check_owner(ctx, window) {
        return SyscallResult::err(e);
    }
    if ctx.intuition.repaint_window(window).is_err() {
        return SyscallResult::err(errors::NO_OBJECT);
    }
    if args.get(1) != 0 {
        let _ = ctx.intuition.composite_to_framebuffer();
    }
    SyscallResult::ok(0)
}

/// `RefreshIntui` — repaint and composite every window.
///
/// Arguments: `flags`.
pub fn refresh_all(args: SyscallArgs, ctx: &SyscallContext<'_>) -> SyscallResult {
    ctx.intuition.repaint_all();
    if args.get(0) != 0 {
        let _ = ctx.intuition.composite_to_framebuffer();
    }
    SyscallResult::ok(0)
}

/// `AddGadget` — attach a gadget to a window.
///
/// Arguments: `window`, `kind`, `rect_ptr` (a user pointer to a
/// [`UserRect`]), `label_ptr`. Returns the gadget handle.
///
/// The rectangle goes through a pointer rather than four registers because
/// the call needs seven values and the ABI has six. See [`UserRect`] for the
/// full reasoning; the short version is that a pointer is validated and four
/// registers are not.
pub fn add_gadget(args: SyscallArgs, ctx: &SyscallContext<'_>) -> SyscallResult {
    let window = WindowId(args.get(0) as u32);
    if let Err(e) = check_owner(ctx, window) {
        return SyscallResult::err(e);
    }
    let kind = match gadget_kind(args.get(1)) {
        Some(k) => k,
        None => return SyscallResult::err(errors::RANGE),
    };
    let bounds = match read_user_rect(args.get(2)) {
        Ok(r) => r.to_rect(),
        Err(e) => return SyscallResult::err(e),
    };
    let label = match crate::copy::copy_string_alloc(args.get(3)) {
        Ok(s) => s,
        Err(e) => return SyscallResult::err(e),
    };
    match ctx.intuition.add_gadget(window, kind, bounds, &label) {
        Ok(id) => SyscallResult::ok(id.0 as u64),
        Err(e) => SyscallResult::err(e),
    }
}

/// `RemoveGadget` — detach a gadget.
///
/// Arguments: `window`, `gadget`. The window is checked because the gadget's
/// owner is the window, and removing a gadget from the wrong window would
/// leave the two disagreeing about what exists.
pub fn remove_gadget(args: SyscallArgs, ctx: &SyscallContext<'_>) -> SyscallResult {
    let window = WindowId(args.get(0) as u32);
    if let Err(e) = check_owner(ctx, window) {
        return SyscallResult::err(e);
    }
    let gadget = vela_kernel_intuition::GadgetId(args.get(1) as u32);
    match ctx.intuition.remove_gadget(gadget) {
        Ok(()) => SyscallResult::ok(0),
        Err(e) => SyscallResult::err(e),
    }
}

/// `BringWindowToFront` — bring a window to the front.
///
/// Arguments: `window`.
pub fn front(args: SyscallArgs, ctx: &SyscallContext<'_>) -> SyscallResult {
    let window = WindowId(args.get(0) as u32);
    match ctx.intuition.window(window) {
        Some(w) => match ctx.intuition.layers().to_front(w.layer) {
            Ok(()) => SyscallResult::ok(0),
            Err(e) => SyscallResult::err(e),
        },
        None => SyscallResult::err(errors::NO_OBJECT),
    }
}

/// `SendWindowToBack` — send a window to the back.
///
/// Arguments: `window`.
pub fn back(args: SyscallArgs, ctx: &SyscallContext<'_>) -> SyscallResult {
    let window = WindowId(args.get(0) as u32);
    match ctx.intuition.window(window) {
        Some(w) => match ctx.intuition.layers().to_back(w.layer) {
            Ok(()) => SyscallResult::ok(0),
            Err(e) => SyscallResult::err(e),
        },
        None => SyscallResult::err(errors::NO_OBJECT),
    }
}

/// `SetGadgetEnabled` — enable or disable a gadget.
///
/// Arguments: `window`, `gadget`, `enabled`.
pub fn set_gadget_enabled(args: SyscallArgs, ctx: &SyscallContext<'_>) -> SyscallResult {
    let window = WindowId(args.get(0) as u32);
    if let Err(e) = check_owner(ctx, window) {
        return SyscallResult::err(e);
    }
    let gadget = vela_kernel_intuition::GadgetId(args.get(1) as u32);
    match ctx.intuition.set_gadget_enabled(gadget, args.get(2) != 0) {
        Ok(()) => SyscallResult::ok(0),
        Err(e) => SyscallResult::err(e),
    }
}

/// Verify that the calling task owns a window.
///
/// A separate function because every mutating handler needs it and getting it
/// wrong in one place would let a program modify another's window through the
/// one handler that forgot.
fn check_owner(ctx: &SyscallContext<'_>, window: WindowId) -> Result<(), i32> {
    match ctx.intuition.window(window) {
        Some(w) if w.owner == ctx.task => Ok(()),
        Some(_) => Err(errors::NOT_OWNER),
        None => Err(errors::NO_OBJECT),
    }
}

/// Decode a gadget kind from the syscall's numeric value.
fn gadget_kind(raw: u64) -> Option<vela_kernel_intuition::GadgetKind> {
    use vela_kernel_intuition::GadgetKind;
    Some(match raw {
        0 => GadgetKind::Button,
        1 => GadgetKind::Label,
        2 => GadgetKind::Toggle,
        3 => GadgetKind::Custom,
        _ => return None,
    })
}

/// Pack a rectangle into one 64-bit return value.
fn pack_rect(r: vela_renderer2d::Rect) -> u64 {
    (r.x as i16 as u16 as u64)
        | ((r.y as i16 as u16 as u64) << 16)
        | ((r.width as u16 as u64) << 32)
        | ((r.height as u16 as u64) << 48)
}

/// Re-exported so a program and the kernel agree on what "every event" means.
pub const IDCMP_ALL: u16 = idcmp::ALL;

/// Re-exported so a program and the kernel agree on the window flags.
pub const CLOSE_GADGET: u32 = window_flags::CLOSEGADGET;
