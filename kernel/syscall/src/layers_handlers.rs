//! The Layers-library syscall handlers.
//!
//! # Why `LAYERS_COMPOSITE` takes a framebuffer pointer but never dereferences one
//!
//! Compositing happens in the kernel, against the framebuffer the display
//! driver mapped — not against anything user space names. So the syscall does
//! not take a buffer at all: it takes the screen's geometry and tells the
//! kernel to composite its own stack into its own framebuffer, and reports the
//! rectangle it touched.
//!
//! The signature is `LAYERS_COMPOSITE(screen) -> touched_rect_packed`, and the
//! reason it returns a packed rectangle rather than writing through a user
//! pointer is that a user pointer would be an address the kernel would have to
//! validate for a value it could compute itself.

use crate::dispatch::SyscallContext;
use crate::numbers::{errors, SyscallArgs, SyscallResult};

/// `CreateLayer` — a standalone layer with its own back buffer.
///
/// Arguments: `x`, `y`, `width`, `height`. Returns the layer handle, or zero
/// on failure.
///
/// A program creates a layer to render into before showing it. The layer is
/// created *invisible* only if the caller says so; a plain layer is visible,
/// because that is what "create a layer and draw in it" means.
pub fn create(args: SyscallArgs, ctx: &SyscallContext<'_>) -> SyscallResult {
    let bounds = vela_renderer2d::Rect::new(
        args.get(0) as i32 as i16 as i32,
        args.get(1) as i32 as i16 as i32,
        args.get(2) as u16 as u32,
        args.get(3) as u16 as u32,
    );
    match ctx.intuition.layers().create_buffered_layer(
        vela_kernel_layers::LayerKind::Offscreen,
        bounds,
        0,
    ) {
        Ok(id) => SyscallResult::ok(id.0 as u64),
        Err(e) => SyscallResult::err(e),
    }
}

/// `DeleteLayer` — destroy a layer.
///
/// Arguments: `layer`.
pub fn destroy(args: SyscallArgs, ctx: &SyscallContext<'_>) -> SyscallResult {
    let layer = crate::ids::LayerId(args.get(0) as u32);
    match ctx.intuition.layers().destroy_layer(layer) {
        Ok(_) => SyscallResult::ok(0),
        Err(e) => SyscallResult::err(e),
    }
}

/// `SetClip` — a layer's clip rectangle.
///
/// Arguments: `layer`, `x`, `y`, `width`, `height`. Returns the effective
/// clip packed as `x | (y << 16) | (width << 32) | (height << 48)`.
///
/// Packed into the return rather than written through a pointer: the kernel
/// knows the answer and the program does not, so a pointer would be a way to
/// make the kernel do extra work and nothing else.
pub fn set_clip(args: SyscallArgs, ctx: &SyscallContext<'_>) -> SyscallResult {
    let layer = crate::ids::LayerId(args.get(0) as u32);
    let clip = vela_renderer2d::Rect::new(
        args.get(1) as i32 as i16 as i32,
        args.get(2) as i32 as i16 as i32,
        args.get(3) as u16 as u32,
        args.get(4) as u16 as u32,
    );
    match ctx.intuition.layers().set_clip(layer, clip) {
        Ok(effective) => SyscallResult::ok(pack_rect(effective)),
        Err(e) => SyscallResult::err(e),
    }
}

/// `BringToFront` — bring a layer to the front of the stack.
///
/// Arguments: `layer`.
pub fn front(args: SyscallArgs, ctx: &SyscallContext<'_>) -> SyscallResult {
    let layer = crate::ids::LayerId(args.get(0) as u32);
    match ctx.intuition.layers().to_front(layer) {
        Ok(()) => SyscallResult::ok(0),
        Err(e) => SyscallResult::err(e),
    }
}

/// `SendToBack` — send a layer to the back of the stack.
///
/// Arguments: `layer`.
pub fn back(args: SyscallArgs, ctx: &SyscallContext<'_>) -> SyscallResult {
    let layer = crate::ids::LayerId(args.get(0) as u32);
    match ctx.intuition.layers().to_back(layer) {
        Ok(()) => SyscallResult::ok(0),
        Err(e) => SyscallResult::err(e),
    }
}

/// `SetLayerVisibility` — show or hide a layer.
///
/// Arguments: `layer`, `visible`.
pub fn set_visible(args: SyscallArgs, ctx: &SyscallContext<'_>) -> SyscallResult {
    let layer = crate::ids::LayerId(args.get(0) as u32);
    let visible = args.get(1) != 0;
    match ctx.intuition.layers().set_visible(layer, visible) {
        Ok(()) => SyscallResult::ok(0),
        Err(e) => SyscallResult::err(e),
    }
}

/// `MoveLayer` — move or resize a layer.
///
/// Arguments: `layer`, `x`, `y`, `width`, `height`. Returns the rectangle the
/// layer used to cover, so the caller can repaint what it vacated.
///
/// The old rectangle comes back because a caller that discards it leaves the
/// layer's previous pixels on screen: nothing else will overwrite them until
/// something else happens to draw there.
pub fn move_layer(args: SyscallArgs, ctx: &SyscallContext<'_>) -> SyscallResult {
    let layer = crate::ids::LayerId(args.get(0) as u32);
    let x = args.get(1) as i32 as i16 as i32;
    let y = args.get(2) as i32 as i16 as i32;
    let w = args.get(3) as u16 as u32;
    let h = args.get(4) as u16 as u32;
    let stack = ctx.intuition.layers();
    let result = if w == 0 || h == 0 {
        // A move is a resize to the current size.
        let Some(_record) = stack.layer(layer) else {
            return SyscallResult::err(errors::NO_OBJECT);
        };
        stack.move_layer(layer, x, y)
    } else {
        let bounds = vela_renderer2d::Rect::new(x, y, w, h);
        // A resize is not done with `?`: the result type here is a syscall
        // result, not a `Result`, so the failure has to be converted
        // explicitly. Resizing before moving is deliberate — the layer's
        // clip is expressed in layer-local coordinates, so a move after a
        // resize lands in the right place and a move before it would have to
        // be redone.
        match stack.resize_layer(layer, bounds) {
            Ok(_) => stack.move_layer(layer, x, y),
            Err(e) => return SyscallResult::err(e),
        }
    };
    match result {
        Ok((old, _new)) => SyscallResult::ok(pack_rect(old)),
        Err(e) => SyscallResult::err(e),
    }
}

/// `CompositeLayers` — composite the stack into the display.
///
/// Arguments: none. Returns the rectangle touched, packed.
///
/// The framebuffer is the kernel's own: a program asking for a composite is
/// asking the window manager to refresh, which is exactly what the desktop
/// does after a window moves. There is no user-supplied destination, so there
/// is no user pointer to validate and no way for a program to draw into an
/// address it does not own.
pub fn composite(_args: SyscallArgs, ctx: &SyscallContext<'_>) -> SyscallResult {
    match ctx.intuition.composite_to_framebuffer() {
        Some(rect) => SyscallResult::ok(pack_rect(rect)),
        // "Nothing was dirty" is a successful no-op, not an error: a
        // compositor that returned an error for a clean screen would make
        // every caller check the dirty flag itself.
        None => SyscallResult::ok(0),
    }
}

/// `ObtainOrder` — the z-order, back to front.
///
/// Arguments: `buffer` (a user pointer to a `u32` array). Returns the number
/// of layers written.
///
/// The array is copied into, bounded by [`MAX_ORDER_ENTRIES`] so that a
/// program asking for a huge buffer cannot make the kernel read or write more
/// than it is willing to.
pub fn order(args: SyscallArgs, ctx: &SyscallContext<'_>) -> SyscallResult {
    let ptr = args.get(0);
    let capacity = args.get(1) as usize;
    if capacity == 0 || capacity > MAX_ORDER_ENTRIES {
        return SyscallResult::err(errors::RANGE);
    }
    let stack = ctx.intuition.layers();
    let all = stack.order();
    let n = all.len().min(capacity);
    let mut buffer = alloc::vec![0u32; n];
    for (slot, layer) in buffer.iter_mut().zip(all.iter()) {
        *slot = layer.0;
    }
    if n == 0 {
        return SyscallResult::ok(0);
    }
    // SAFETY: the destination is a user pointer validated and probed by
    // `copy_to_user`; the source is a kernel-owned `Vec` sized to `n`.
    match unsafe {
        crate::copy::copy_to_user(ptr, buffer.as_ptr().cast(), n * core::mem::size_of::<u32>())
    } {
        Ok(_) => SyscallResult::ok(n as u64),
        Err(e) => SyscallResult::err(e),
    }
}

/// `WhichLayer` — the topmost layer at a point.
///
/// Arguments: `x`, `y`. Returns the layer handle, or zero for the backdrop.
///
/// A hit-test belongs to the window manager, not to the program: a program
/// that hit-tested its own idea of the z-order would disagree with the one
/// the events were routed through, and would deliver its own click twice.
pub fn topmost(args: SyscallArgs, ctx: &SyscallContext<'_>) -> SyscallResult {
    let x = args.get(0) as i32 as i16 as i32;
    let y = args.get(1) as i32 as i16 as i32;
    match ctx.intuition.layers().topmost_at(x, y) {
        Some(layer) => SyscallResult::ok(layer.0 as u64),
        None => SyscallResult::ok(0),
    }
}

/// The largest z-order a program may ask to be told about.
///
/// Two hundred is far past anything v0.1.0 opens, and it bounds the copy a
/// single syscall can make.
pub const MAX_ORDER_ENTRIES: usize = 256;

/// Pack a rectangle into one 64-bit return value.
fn pack_rect(r: vela_renderer2d::Rect) -> u64 {
    (r.x as i16 as u16 as u64)
        | ((r.y as i16 as u16 as u64) << 16)
        | ((r.width as u16 as u64) << 32)
        | ((r.height as u16 as u64) << 48)
}
