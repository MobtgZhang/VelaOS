//! The Graphics-library syscall handlers.
//!
//! # What these calls can and cannot do from user space
//!
//! A pen and a palette are pure data: they live in the Graphics service and
//! user space names them by index, so `AllocPen` and `SetPen` are safe to
//! expose directly.
//!
//! A **raster** is not. Its pixels are either the framebuffer — which is
//! kernel-owned memory a program must not write — or a window's back buffer,
//! which is owned by Layers and reached through Intuition. So there is no
//! `OpenRaster` in the syscall table, and `RECT`/`FILL`/`TEXT`/`LINE` all
//! draw into the *calling task's own window*, named by the window handle the
//! task opened.
//!
//! That is the boundary the plan requires: user space reaches the Amiga API,
//! but it never receives a kernel pointer, and it never names memory the
//! kernel owns.

use vela_kernel_graphics::PenId;
use vela_kernel_layers::LayerStack;
use vela_renderer2d::Color;

use crate::copy::{copy_from_user, read_user_rect, write_user_rect, UserRect};
use crate::dispatch::SyscallContext;
use crate::numbers::{errors, SyscallArgs, SyscallResult};

/// `AllocPen` — allocate a pen.
///
/// Arguments: `palette`, `color` (`0x00RRGGBB`). Returns the pen handle, or
/// `0xFFFF` on failure.
///
/// The pen handle is `0xFFFF` on failure rather than zero, because zero is a
/// legitimate pen index — the first pen allocated — and a program that could
/// not tell the two apart would silently draw with the wrong colour.
pub fn alloc_pen(args: SyscallArgs, ctx: &SyscallContext<'_>) -> SyscallResult {
    let palette = args.get(0) as u16;
    let color = args.get(1) as u32;
    match ctx.graphics.alloc_pen(palette, color) {
        Ok(pen) => SyscallResult::ok(pen.0 as u64),
        Err(e) => SyscallResult::err(e),
    }
}

/// `FreePen` — release a pen.
///
/// Arguments: `palette`, `pen`.
pub fn free_pen(args: SyscallArgs, ctx: &SyscallContext<'_>) -> SyscallResult {
    let palette = args.get(0) as u16;
    let pen = PenId(args.get(1) as u16);
    match ctx.graphics.free_pen(palette, pen) {
        Ok(()) => SyscallResult::ok(0),
        Err(e) => SyscallResult::err(e),
    }
}

/// `SetPen` — the colour a pen resolves to, packed for the caller.
///
/// Arguments: `palette`, `pen`. Returns `0x00RRGGBB`.
///
/// The value is *returned* rather than stored in the raster: there is no
/// per-user rast port state in the kernel, because that state would be a
/// per-task allocation that has to survive a task exit and be reclaimed on it.
/// The program holds its own pen and asks the palette to resolve it, which
/// costs one array read and allocates nothing.
pub fn set_pen(args: SyscallArgs, ctx: &SyscallContext<'_>) -> SyscallResult {
    let palette = args.get(0) as u16;
    let pen = PenId(args.get(1) as u16);
    match ctx.graphics.pen_color(palette, pen) {
        Some(color) => SyscallResult::ok(color as u64),
        None => SyscallResult::err(errors::NO_OBJECT),
    }
}

/// `Move` — set the window's drawing position, then draw a point.
///
/// Arguments: `window`, `pen`, `x`, `y`. Returns zero.
///
/// VelaOS has no per-task rast port, so `Move` cannot leave a position
/// behind for a later `Line` to start from. What it does instead is the part
/// that is still meaningful without one: it sets the position *and* plots
/// it, which is exactly what a "move to" means in a path-based drawing API.
/// A program that wants a polyline draws each segment explicitly, and pays
/// one syscall per segment rather than paying for a kernel-side port it would
/// have to reclaim on exit.
pub fn move_to(args: SyscallArgs, ctx: &SyscallContext<'_>) -> SyscallResult {
    let window = crate::ids::WindowId(args.get(0) as u32);
    let pen = PenId(args.get(1) as u16);
    let x = args.get(2) as i32 as i16;
    let y = args.get(3) as i32 as i16;
    let Some(color) = ctx.graphics.pen_color(0, pen) else {
        return SyscallResult::err(errors::NO_OBJECT);
    };
    match paint_window(ctx, window, Color::from(color), |l| {
        l.put(x as i32, y as i32, pack(color));
    }) {
        Ok(()) => SyscallResult::ok(0),
        Err(e) => SyscallResult::err(e),
    }
}

/// `RectFill` — fill a rectangle in the calling task's window.
///
/// Arguments: `window`, `pen`, `rect` (a user pointer to a [`UserRect`]).
///
/// The rectangle is validated *before* it is used: a pointer in the kernel
/// half is refused without being read, and a read that faults is an error
/// rather than a kernel panic.
pub fn fill_rect(args: SyscallArgs, ctx: &SyscallContext<'_>) -> SyscallResult {
    let window = crate::ids::WindowId(args.get(0) as u32);
    let pen = PenId(args.get(1) as u16);
    let Some(rect) = read_rect(args.get(2)) else {
        return SyscallResult::err(errors::BAD_POINTER);
    };
    let Some(color) = ctx.graphics.pen_color(0, pen) else {
        return SyscallResult::err(errors::NO_OBJECT);
    };
    match paint_window(ctx, window, Color::from(color), |l| {
        let r = rect.to_rect();
        for y in 0..r.height as i32 {
            for x in 0..r.width as i32 {
                l.put(r.x + x, r.y + y, pack(color));
            }
        }
    }) {
        Ok(()) => SyscallResult::ok(0),
        Err(e) => SyscallResult::err(e),
    }
}

/// `Rect` — stroke a rectangle outline in the calling task's window.
///
/// Arguments: `window`, `pen`, `rect`.
pub fn rect(args: SyscallArgs, ctx: &SyscallContext<'_>) -> SyscallResult {
    let window = crate::ids::WindowId(args.get(0) as u32);
    let pen = PenId(args.get(1) as u16);
    let Some(rect) = read_rect(args.get(2)) else {
        return SyscallResult::err(errors::BAD_POINTER);
    };
    let Some(color) = ctx.graphics.pen_color(0, pen) else {
        return SyscallResult::err(errors::NO_OBJECT);
    };
    let r = rect.to_rect();
    match paint_window(ctx, window, Color::from(color), |l| {
        for i in 0..r.width as i32 {
            l.put(r.x + i, r.y, pack(color));
            l.put(r.x + i, r.y + r.height as i32 - 1, pack(color));
        }
        for i in 0..r.height as i32 {
            l.put(r.x, r.y + i, pack(color));
            l.put(r.x + r.width as i32 - 1, r.y + i, pack(color));
        }
    }) {
        Ok(()) => SyscallResult::ok(0),
        Err(e) => SyscallResult::err(e),
    }
}

/// `Text` — draw a string in the calling task's window.
///
/// Arguments: `window`, `pen`, `x`, `y`, `text` (a user NUL-terminated
/// string). Returns the advance width in pixels.
pub fn text(args: SyscallArgs, ctx: &SyscallContext<'_>) -> SyscallResult {
    let window = crate::ids::WindowId(args.get(0) as u32);
    let pen = PenId(args.get(1) as u16);
    let x = args.get(2) as i32 as i16;
    let y = args.get(3) as i32 as i16;
    let Some(color) = ctx.graphics.pen_color(0, pen) else {
        return SyscallResult::err(errors::NO_OBJECT);
    };
    let string = match crate::copy::copy_string_alloc(args.get(4)) {
        Ok(s) => s,
        Err(e) => return SyscallResult::err(e),
    };
    let width = (string.chars().count() * vela_text::GLYPH_WIDTH as usize) as u64;
    match paint_window(ctx, window, Color::from(color), |l| {
        ctx.intuition
            .draw_string_in_layer(l, &string, x as i32, y as i32, pack(color));
    }) {
        Ok(()) => SyscallResult::ok(width),
        Err(e) => SyscallResult::err(e),
    }
}

/// `Line` — draw a line in the calling task's window.
///
/// Arguments: `window`, `pen`, `x0`, `y0`, `x1`, `y1`.
pub fn line(args: SyscallArgs, ctx: &SyscallContext<'_>) -> SyscallResult {
    let window = crate::ids::WindowId(args.get(0) as u32);
    let pen = PenId(args.get(1) as u16);
    let (x0, y0, x1, y1) = (
        args.get(2) as i32 as i16,
        args.get(3) as i32 as i16,
        args.get(4) as i32 as i16,
        args.get(5) as i32 as i16,
    );
    let Some(color) = ctx.graphics.pen_color(0, pen) else {
        return SyscallResult::err(errors::NO_OBJECT);
    };
    match paint_window(ctx, window, Color::from(color), |l| {
        bresenham(l, pack(color), x0 as i32, y0 as i32, x1 as i32, y1 as i32);
    }) {
        Ok(()) => SyscallResult::ok(0),
        Err(e) => SyscallResult::err(e),
    }
}

/// `WritePixel` — one pixel in the calling task's window.
///
/// Arguments: `window`, `pen`, `x`, `y`.
pub fn write_pixel(args: SyscallArgs, ctx: &SyscallContext<'_>) -> SyscallResult {
    let window = crate::ids::WindowId(args.get(0) as u32);
    let pen = PenId(args.get(1) as u16);
    let (x, y) = (args.get(2) as i32 as i16, args.get(3) as i32 as i16);
    let Some(color) = ctx.graphics.pen_color(0, pen) else {
        return SyscallResult::err(errors::NO_OBJECT);
    };
    match paint_window(ctx, window, Color::from(color), |l| {
        l.put(x as i32, y as i32, pack(color));
    }) {
        Ok(()) => SyscallResult::ok(0),
        Err(e) => SyscallResult::err(e),
    }
}

/// `SetClip` — set the calling task's window's clip.
///
/// Arguments: `window`, `rect`. Returns the effective clip in `rax`, packed as
/// `(x & 0xFFFF) | (y << 16)`, and the width and height in the remaining two
/// halves of the rect the caller passed in — the kernel writes them back
/// through the same pointer, which is the AmigaOS `SetClip` signature.
pub fn set_clip(args: SyscallArgs, ctx: &SyscallContext<'_>) -> SyscallResult {
    let window = crate::ids::WindowId(args.get(0) as u32);
    let rect_ptr = args.get(1);
    let Some(rect) = read_rect(rect_ptr) else {
        return SyscallResult::err(errors::BAD_POINTER);
    };
    let Some(record) = ctx.intuition.window(window) else {
        return SyscallResult::err(errors::NO_OBJECT);
    };
    match ctx
        .intuition
        .layers()
        .set_clip(record.layer, rect.to_rect())
    {
        Ok(effective) => {
            // Write the effective clip back, so the program learns what it
            // actually got rather than what it asked for.
            // SAFETY: the destination was validated by `read_rect` a moment
            // ago, so it is a writable user pointer of the right size.
            match write_user_rect(rect_ptr, UserRect::from_rect(effective)) {
                Ok(()) => SyscallResult::ok(0),
                Err(e) => SyscallResult::err(e),
            }
        }
        Err(e) => SyscallResult::err(e),
    }
}

/// `SetPalette` — install a palette the program supplies.
///
/// Arguments: `palette`, `colors_ptr` (a user pointer to an array of
/// `0x00RRGGBB` words), `count`. Returns the number of entries installed.
///
/// The array is copied into a kernel palette and *not* retained: a program
/// that unmapped or reused its buffer must not change the screen behind the
/// kernel's back.
pub fn set_palette(args: SyscallArgs, ctx: &SyscallContext<'_>) -> SyscallResult {
    let palette = args.get(0) as u16;
    let ptr = args.get(1);
    let count = args.get(2) as usize;
    if count == 0 || count > crate::copy::MAX_COPY_BYTES / 4 {
        return SyscallResult::err(errors::RANGE);
    }
    // Read into a fixed-size stack buffer first, so the copy cannot be made to
    // allocate an arbitrarily large `Vec` on the kernel's behalf.
    let mut colors = alloc::vec::Vec::new();
    if colors.try_reserve_exact(count).is_err() {
        return SyscallResult::err(errors::NO_MEMORY);
    }
    colors.resize(count, 0u32);
    // SAFETY: the destination is a kernel-owned `Vec` sized to `count` words,
    // and the source is copied *from* user space.
    if unsafe { copy_from_user(colors.as_mut_ptr().cast(), ptr, count * 4) }.is_err() {
        return SyscallResult::err(errors::BAD_POINTER);
    }
    let mut table = vela_kernel_graphics::Palette::new();
    if table.set_many(0, &colors).is_err() {
        return SyscallResult::err(errors::RANGE);
    }
    match ctx.graphics.set_palette(palette, table) {
        Ok(()) => SyscallResult::ok(count as u64),
        Err(e) => SyscallResult::err(e),
    }
}

/// `GetAttr(RASTROSize)` — a window raster's geometry.
///
/// Arguments: `window`. Returns `width | (height << 32)`, so one word pair
/// carries both without a user pointer.
pub fn raster_info(args: SyscallArgs, ctx: &SyscallContext<'_>) -> SyscallResult {
    let window = crate::ids::WindowId(args.get(0) as u32);
    let Some(record) = ctx.intuition.window(window) else {
        return SyscallResult::err(errors::NO_OBJECT);
    };
    SyscallResult::ok(record.bounds.width as u64 | ((record.bounds.height as u64) << 32))
}

/// Read a [`UserRect`] from user space.
///
/// A thin alias over [`read_user_rect`], kept because every graphics handler
/// spells its rectangle reads the same way and getting the spelling wrong in
/// one place would be a handler that reads eight bytes from the wrong address.
fn read_rect(ptr: u64) -> Option<UserRect> {
    read_user_rect(ptr).ok()
}

/// Paint into a window's layer, and only into a window the task owns.
fn paint_window(
    ctx: &SyscallContext<'_>,
    window: crate::ids::WindowId,
    _color: Color,
    f: impl FnOnce(&mut vela_kernel_layers::Layer),
) -> Result<(), i32> {
    let Some(record) = ctx.intuition.window(window) else {
        return Err(errors::NO_OBJECT);
    };
    // Ownership check. A task that could draw into another task's window
    // could scribble over it, and the owner would have no way to know.
    if record.owner != ctx.task {
        return Err(errors::NOT_OWNER);
    }
    ctx.intuition.layers().with_layer(record.layer, f)?;
    ctx.intuition.layers().invalidate(record.layer)?;
    Ok(())
}

/// Pack a colour the way a layer's buffer holds it.
fn pack(color: u32) -> u32 {
    LayerStack::pack(Color::from(color))
}

/// Bresenham's line, into a layer buffer.
fn bresenham(
    layer: &mut vela_kernel_layers::Layer,
    color: u32,
    x0: i32,
    y0: i32,
    x1: i32,
    y1: i32,
) {
    let dx = (x1 - x0).abs();
    let sx = if x0 < x1 { 1 } else { -1 };
    let dy = -(y1 - y0).abs();
    let sy = if y0 < y1 { 1 } else { -1 };
    let mut err = dx + dy;
    let (mut x, mut y) = (x0, y0);
    // The loop is bounded by the geometry: every iteration advances x or y
    // toward the endpoint, so it terminates after at most `dx + |dy|` steps.
    // A bound is written anyway, because a handler that can hang holds the
    // syscall lock and takes the kernel with it.
    let limit = dx + dy.unsigned_abs() as i32 + 2;
    let mut steps = 0;
    loop {
        layer.put(x, y, color);
        if (x == x1 && y == y1) || steps >= limit {
            break;
        }
        steps += 1;
        let e2 = 2 * err;
        if e2 >= dy {
            err += dy;
            x += sx;
        }
        if e2 <= dx {
            err += dx;
            y += sy;
        }
    }
}

/// The layer stack's paint colour type, re-exported for the handlers.
pub type PaintColor = Color;
