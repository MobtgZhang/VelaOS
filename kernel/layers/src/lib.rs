// Layers: z-ordered, clipped compositing of windows onto a screen.
//
// # Why layers exist at all
//
// A window is not a rectangle of pixels the window owns. It is a *layer*: a
// back buffer plus a region on screen, plus a position in the z-order. Three
// things follow from that, and all three are why this crate exists:
//
// 1. **Overlapping windows must not corrupt each other.** A window that
//    scribbled straight onto the screen would erase whatever was under it, and
//    uncovering it would require the whole screen to be redrawn. So every
//    window draws into its own back buffer and is copied onto the screen in
//    z-order at composite time.
// 2. **A window may be partly off-screen or under another window.** Its layer
//    records where that is, so that the input router can ask "is this click
//    for me?" without looking at pixels.
// 3. **Damage is a rectangle, not a screen.** Moving or resizing a window
//    invalidates the union of its old and new regions; the composite pass
//    redraws that union and nothing else.
//
// # What v0.1.0 does *not* do
//
// There is no alpha blending, no dirty-region tracking per pixel, and no
// async compositor. The screen is composited synchronously whenever something
// is marked dirty. On a software renderer at 1280x800 that is a full-screen
// copy per change, which is roughly four megabytes of `memcpy` — fast enough to
// be invisible, and much simpler than an incremental compositor.
//
// # The layering model, precisely
//
// * Layers are ordered back-to-front. Index 0 is the backmost.
// * The **back** layer is the screen itself: it is the backdrop every other
//   layer composites onto, and it is never occluded.
// * `front(n)` / `back(n)` move a layer within the stack, excluding the screen
//   layer, exactly as `layers.library`'s `BringToFront`/`SendToBack` do.
// * A layer's clip rectangle is the intersection of its own bounds with
//   everything above it on the stack, so a layer knows exactly which of its
//   pixels are visible. That is what makes hit-testing correct without asking
//   the screen what is on top.

#![no_std]

extern crate alloc;

use alloc::vec::Vec;

use spin::Mutex;

use vela_renderer2d::{Color, Rect};

/// The errors Layers returns.
pub mod errors {
    /// The call succeeded.
    pub const OK: i32 = 0;
    /// No layer of that handle exists.
    pub const NO_OBJECT: i32 = -2;
    /// The layer is already in the stack.
    pub const OBJECT_EXISTS: i32 = -3;
    /// A memory allocation failed.
    pub const NO_MEMORY: i32 = -12;
    /// The geometry is invalid.
    pub const BAD_RECT: i32 = -13;
}

/// A handle to a layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LayerId(pub u32);

impl LayerId {
    /// The id of "no layer".
    pub const NONE: LayerId = LayerId(0);

    /// True when this id names no layer.
    pub const fn is_none(self) -> bool {
        self.0 == 0
    }
}

/// What a layer's pixel storage is.
///
/// The layer does not own its pixels. It knows how big they are and how they
/// are addressed, and the caller supplies the actual buffer when it composes.
/// That keeps one ownership rule: whoever allocated a buffer frees it, and the
/// layer is just the geometry plus the z-order position.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayerKind {
    /// The screen's own layer: full-screen, never occluded.
    Screen,
    /// A window layer: sized by the window.
    Window,
    /// A hidden or offscreen layer: sized, but not composited.
    Offscreen,
}

/// One layer.
///
/// # Coordinate systems
///
/// A layer has *two* rectangles and they are in different spaces, which is the
/// single most important thing to get right here:
///
/// * [`Self::bounds`] — the layer's size, and where it sits on screen.
/// * [`Self::clip`] — the drawable part of the layer, in the layer's **own**
///   coordinates, which start at `(0, 0)`.
///
/// So `clip` never has the layer's screen offset in it, and `to_local` is a
/// subtraction with no correction for `clip.x`. An earlier version of this
/// struct initialised `clip` to `bounds`, which silently put the clip at the
/// screen offset and made `to_local` return `None` for the top-left pixel of
/// every window.
#[derive(Debug, Clone)]
pub struct Layer {
    /// The handle user space knows this layer by.
    pub id: LayerId,
    /// What the layer is.
    pub kind: LayerKind,
    /// The layer's size, and its screen position.
    pub bounds: Rect,
    /// The drawable part of the layer, in the layer's own coordinates.
    pub clip: Rect,
    /// The screen position of the layer's top-left corner.
    ///
    /// Always equal to `(bounds.x, bounds.y)`; a separate field would be a
    /// second source of truth that could disagree with the first.
    pub position: (i32, i32),
    /// Whether the layer is drawn at all.
    pub visible: bool,
    /// The layer's own back buffer, when it has one.
    ///
    /// `None` for the screen layer, whose pixels are the framebuffer.
    pub buffer: Option<Vec<u32>>,
    /// The buffer's stride in pixels.
    pub stride: u32,
    /// Whether the layer has been marked for composite.
    pub dirty: bool,
}

impl Layer {
    /// A layer with the given geometry and no buffer.
    pub fn new(id: LayerId, kind: LayerKind, bounds: Rect) -> Self {
        Self {
            id,
            kind,
            clip: Rect::new(0, 0, bounds.width, bounds.height),
            bounds,
            position: (bounds.x, bounds.y),
            visible: kind != LayerKind::Offscreen,
            buffer: None,
            stride: bounds.width,
            dirty: true,
        }
    }

    /// A layer with its own back buffer.
    ///
    /// The buffer is allocated uninitialised-by-contract: every byte is set to
    /// `fill`, because a window that is shown before it has drawn anything
    /// would otherwise show whatever was on the heap.
    pub fn with_buffer(id: LayerId, kind: LayerKind, bounds: Rect, fill: u32) -> Self {
        let stride = bounds.width;
        let len = (stride as usize) * (bounds.height as usize);
        let mut layer = Self::new(id, kind, bounds);
        layer.buffer = Some(alloc::vec![fill; len]);
        layer.stride = stride;
        layer
    }

    /// Give the layer a back buffer.
    pub fn alloc_buffer(&mut self, fill: u32) {
        let len = (self.stride as usize) * (self.bounds.height as usize);
        self.buffer = Some(alloc::vec![fill; len]);
        self.dirty = true;
    }

    /// The buffer's length in pixels.
    pub fn pixel_count(&self) -> usize {
        (self.stride as usize) * (self.bounds.height as usize)
    }

    /// Read a pixel from the layer's buffer.
    pub fn get(&self, x: i32, y: i32) -> Option<u32> {
        let b = self.buffer.as_ref()?;
        if x < 0 || y < 0 {
            return None;
        }
        let (x, y) = (x as u32, y as u32);
        if x >= self.bounds.width || y >= self.bounds.height {
            return None;
        }
        b.get((y as usize) * (self.stride as usize) + x as usize)
            .copied()
    }

    /// Write a pixel into the layer's buffer and mark the layer dirty.
    ///
    /// Returns false for a pixel outside the layer, and for a layer with no
    /// buffer. Both are refused rather than clipped, because a drawing call
    /// that silently lost its pixel would look like a renderer bug.
    pub fn put(&mut self, x: i32, y: i32, value: u32) -> bool {
        if x < 0 || y < 0 {
            return false;
        }
        let (x, y) = (x as u32, y as u32);
        if x >= self.bounds.width || y >= self.bounds.height {
            return false;
        }
        let index = (y as usize) * (self.stride as usize) + x as usize;
        let Some(buffer) = self.buffer.as_mut() else {
            return false;
        };
        if index >= buffer.len() {
            return false;
        }
        buffer[index] = value;
        self.dirty = true;
        true
    }

    /// Fill the whole buffer with `value`.
    pub fn clear(&mut self, value: u32) {
        if let Some(b) = self.buffer.as_mut() {
            for v in b.iter_mut() {
                *v = value;
            }
        }
        self.dirty = true;
    }

    /// The rectangle the layer occupies on screen, after clipping.
    ///
    /// The clip is in layer coordinates, so it has to be placed at the
    /// layer's screen position rather than used directly.
    pub fn screen_rect(&self) -> Rect {
        Rect::new(
            self.position.0 + self.clip.x,
            self.position.1 + self.clip.y,
            self.clip.width,
            self.clip.height,
        )
    }

    /// True when `(x, y)` in screen coordinates is inside the layer.
    pub fn contains_screen(&self, x: i32, y: i32) -> bool {
        self.screen_rect().contains(x, y)
    }

    /// Convert a screen coordinate into coordinates relative to the layer's clip.
    ///
    /// The result is what a program drawing into a clipped window indexes
    /// with: the clip's top-left is `(0, 0)`, not the buffer's. That is the
    /// AmigaOS convention, and it is the only one that survives a window
    /// whose clip is not its whole bounds — otherwise every drawing call in
    /// such a window would have to subtract the clip offset itself, and one
    /// that forgot would draw in the wrong place.
    ///
    /// Returns `None` when the point is outside the clipped area, which is
    /// what a hit-test needs: a click outside the window is not the window's
    /// event, and reporting it as one is how a click ends up delivered twice.
    pub fn to_local(&self, screen_x: i32, screen_y: i32) -> Option<(i32, i32)> {
        let origin = self.screen_rect();
        let local = (screen_x - origin.x, screen_y - origin.y);
        // The test is against the clip's *size*, not against the clip
        // rectangle: `local` is already measured from the clip's top-left, so
        // testing it against the clip's own x/y would compare it against the
        // offset twice and reject every point.
        if local.0 >= 0
            && local.1 >= 0
            && local.0 < self.clip.width as i32
            && local.1 < self.clip.height as i32
        {
            Some(local)
        } else {
            None
        }
    }

    /// Convert a screen coordinate into a buffer index.
    ///
    /// Unlike [`Self::to_local`], this is relative to the layer's *buffer*
    /// origin rather than its clip, because it indexes storage.
    pub fn to_buffer(&self, screen_x: i32, screen_y: i32) -> Option<(usize, usize)> {
        let (x, y) = (
            screen_x - self.position.0 - self.clip.x,
            screen_y - self.position.1 - self.clip.y,
        );
        if x < 0 || y < 0 {
            return None;
        }
        if x as u32 >= self.bounds.width || y as u32 >= self.bounds.height {
            return None;
        }
        Some((x as usize, y as usize))
    }
}

/// The layer stack.
///
/// The stack is a `Vec` in z-order. A `Vec` rather than a linked list because
/// every composite, every move and every hit-test walks it front to back, and
/// a list would make each of those a pointer chase per layer.
pub struct LayerStack {
    layers: Mutex<Vec<Layer>>,
    next_id: Mutex<u32>,
    /// The screen's geometry, so a layer cannot be positioned off it.
    screen: Rect,
}

impl core::fmt::Debug for LayerStack {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("LayerStack")
            .field("screen", &self.screen)
            .field("layers", &self.layers.lock().len())
            .finish()
    }
}

impl LayerStack {
    /// A stack over a screen of `width` by `height`.
    pub fn new(width: u32, height: u32) -> Self {
        Self {
            layers: Mutex::new(Vec::new()),
            next_id: Mutex::new(1),
            screen: Rect::new(0, 0, width, height),
        }
    }

    /// The screen's geometry.
    pub fn screen(&self) -> Rect {
        self.screen
    }

    /// Create the screen's own layer and put it at the back of the stack.
    ///
    /// The screen layer has no back buffer: its pixels are the framebuffer.
    /// It exists so that the z-order has a well-defined bottom — a layer sent
    /// to the back lands just above it rather than behind something that is
    /// not there.
    pub fn create_screen_layer(&self) -> Result<LayerId, i32> {
        let id = self.create_layer(LayerKind::Screen, self.screen)?;
        self.reorder(id, |_| 0)?;
        Ok(id)
    }

    /// Create a layer and add it to the front of the stack.
    ///
    /// Handles are never reused while a layer lives, for the same reason port
    /// ids are not: a stale window handle must not name a different window.
    pub fn create_layer(&self, kind: LayerKind, bounds: Rect) -> Result<LayerId, i32> {
        if bounds.width == 0 || bounds.height == 0 {
            return Err(errors::BAD_RECT);
        }
        let id = {
            let mut next = self.next_id.lock();
            let id = *next;
            *next += 1;
            LayerId(id)
        };
        let mut layers = self.layers.lock();
        layers.push(Layer::new(id, kind, bounds));
        Ok(id)
    }

    /// Create a layer with its own back buffer.
    pub fn create_buffered_layer(
        &self,
        kind: LayerKind,
        bounds: Rect,
        fill: u32,
    ) -> Result<LayerId, i32> {
        if bounds.width == 0 || bounds.height == 0 {
            return Err(errors::BAD_RECT);
        }
        let id = {
            let mut next = self.next_id.lock();
            let id = *next;
            *next += 1;
            LayerId(id)
        };
        let mut layers = self.layers.lock();
        layers.push(Layer::with_buffer(id, kind, bounds, fill));
        Ok(id)
    }

    /// Destroy a layer and return its buffer.
    pub fn destroy_layer(&self, id: LayerId) -> Result<Layer, i32> {
        let mut layers = self.layers.lock();
        let index = layers
            .iter()
            .position(|l| l.id == id)
            .ok_or(errors::NO_OBJECT)?;
        Ok(layers.remove(index))
    }

    /// True when a layer of that handle exists.
    pub fn exists(&self, id: LayerId) -> bool {
        self.layers.lock().iter().any(|l| l.id == id)
    }

    /// How many layers the stack holds.
    pub fn len(&self) -> usize {
        self.layers.lock().len()
    }

    /// True when the stack is empty.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// A snapshot of one layer.
    pub fn layer(&self, id: LayerId) -> Option<Layer> {
        self.layers.lock().iter().find(|l| l.id == id).cloned()
    }

    /// Move a layer to a new screen position.
    ///
    /// Returns the layer's old and new screen rectangles, which is what the
    /// caller must redraw: the union of the two. Moving a window without
    /// redrawing the union leaves a ghost of it behind.
    pub fn move_layer(&self, id: LayerId, x: i32, y: i32) -> Result<(Rect, Rect), i32> {
        let mut layers = self.layers.lock();
        let layer = layers
            .iter_mut()
            .find(|l| l.id == id)
            .ok_or(errors::NO_OBJECT)?;
        let old = layer.screen_rect();
        layer.position = (x, y);
        layer.dirty = true;
        let new = layer.screen_rect();
        Ok((old, new))
    }

    /// Resize a layer.
    ///
    /// The old rectangle is returned so the caller can repaint what the layer
    /// used to cover. A resize that discards the old rectangle leaks it: the
    /// pixels stay on the screen until something else happens to draw over
    /// them.
    ///
    /// The back buffer is re-strided and what still fits is copied across. A
    /// shrink that dropped the buffer would make the window go blank without
    /// the program ever being told to redraw, which is not a shrink anyone
    /// asked for.
    pub fn resize_layer(&self, id: LayerId, bounds: Rect) -> Result<Rect, i32> {
        if bounds.width == 0 || bounds.height == 0 {
            return Err(errors::BAD_RECT);
        }
        let mut layers = self.layers.lock();
        let layer = layers
            .iter_mut()
            .find(|l| l.id == id)
            .ok_or(errors::NO_OBJECT)?;
        let old = layer.screen_rect();
        if let Some(old_buffer) = layer.buffer.take() {
            let old_stride = layer.stride as usize;
            let old_height = layer.bounds.height as usize;
            let old_width = layer.bounds.width as usize;
            let new_stride = bounds.width as usize;
            let new_height = bounds.height as usize;
            let mut buffer = alloc::vec![0u32; new_stride * new_height];
            let copy_rows = old_height.min(new_height);
            let copy_cols = old_width.min(bounds.width as usize);
            for row in 0..copy_rows {
                for col in 0..copy_cols {
                    buffer[row * new_stride + col] = old_buffer[row * old_stride + col];
                }
            }
            layer.buffer = Some(buffer);
            layer.stride = bounds.width;
        }
        layer.bounds = bounds;
        layer.clip = Rect::new(0, 0, bounds.width, bounds.height);
        layer.dirty = true;
        Ok(old)
    }

    /// Set a layer's clip rectangle, in the layer's own coordinates.
    ///
    /// The clip is clamped to the layer's size: a clip larger than the layer
    /// would let the layer draw outside itself. The screen offset is *not*
    /// subtracted — the caller passes layer coordinates, which is what makes
    /// the same call work for every layer regardless of where it is.
    pub fn set_clip(&self, id: LayerId, clip: Rect) -> Result<Rect, i32> {
        let mut layers = self.layers.lock();
        let layer = layers
            .iter_mut()
            .find(|l| l.id == id)
            .ok_or(errors::NO_OBJECT)?;
        let full = Rect::new(0, 0, layer.bounds.width, layer.bounds.height);
        let effective = full.intersect(&clip).unwrap_or(Rect::new(0, 0, 0, 0));
        layer.clip = effective;
        layer.dirty = true;
        Ok(effective)
    }

    /// Bring a layer to the front of the stack.
    pub fn to_front(&self, id: LayerId) -> Result<(), i32> {
        // `front(n)` is a *relative* move, and `to_front` is the `n`-large
        // case: the layer goes to the last slot.
        self.reorder(id, |_| usize::MAX)
    }

    /// Send a layer to the back of the stack.
    ///
    /// The back is *just above the screen layer*: the screen layer is the
    /// backdrop, and a layer behind it would be invisible. So the target is
    /// index one when a screen layer is at index zero, and index zero when the
    /// stack has no screen layer — which is the case in the hosted tests,
    /// where the framebuffer *is* the backdrop and there is nothing to be
    /// behind.
    ///
    /// The screen layer's position is read *before* the stack is locked: the
    /// lock is a spin mutex, and reading the stack from inside the reorder
    /// closure would take it a second time and deadlock.
    pub fn to_back(&self, id: LayerId) -> Result<(), i32> {
        let has_screen = self.screen_layer_is_at(0);
        self.reorder(id, move |_| usize::from(has_screen))
    }

    /// Move a layer to an explicit index in the z-order.
    ///
    /// `target` receives the stack's length, so a caller can express "the last
    /// slot" without knowing the length. A target beyond the end is clamped
    /// rather than refused: asking for the front when the stack is shorter than
    /// the caller thought is a benign race, not an error.
    pub fn reorder(&self, id: LayerId, target: impl Fn(usize) -> usize) -> Result<(), i32> {
        let mut layers = self.layers.lock();
        let len = layers.len();
        if len == 0 {
            return Err(errors::NO_OBJECT);
        }
        let from = layers
            .iter()
            .position(|l| l.id == id)
            .ok_or(errors::NO_OBJECT)?;
        let to = target(len).min(len - 1);
        if from == to {
            return Ok(());
        }
        let layer = layers.remove(from);
        layers.insert(to, layer);
        // Every layer's visible region may have changed, not just the moved
        // one: a layer that came from behind now covers whatever was in front
        // of it. Marking only the moved layer would leave the layers it now
        // covers showing stale pixels on top of it.
        for l in layers.iter_mut() {
            l.dirty = true;
        }
        Ok(())
    }

    /// Show or hide a layer.
    pub fn set_visible(&self, id: LayerId, visible: bool) -> Result<(), i32> {
        let mut layers = self.layers.lock();
        let layer = layers
            .iter_mut()
            .find(|l| l.id == id)
            .ok_or(errors::NO_OBJECT)?;
        if layer.visible == visible {
            return Ok(());
        }
        layer.visible = visible;
        layer.dirty = true;
        for l in layers.iter_mut() {
            l.dirty = true;
        }
        Ok(())
    }

    /// The z-order, back to front.
    pub fn order(&self) -> Vec<LayerId> {
        self.layers.lock().iter().map(|l| l.id).collect()
    }

    /// Whether the layer at `index` is the screen backdrop.
    fn screen_layer_is_at(&self, index: usize) -> bool {
        self.layers
            .lock()
            .get(index)
            .map(|l| l.kind == LayerKind::Screen)
            .unwrap_or(false)
    }

    /// The index of a layer in the z-order.
    pub fn index_of(&self, id: LayerId) -> Option<usize> {
        self.layers.lock().iter().position(|l| l.id == id)
    }

    /// The topmost layer containing `(x, y)` in screen coordinates.
    ///
    /// Walking front to back is the whole point: the answer is the *first*
    /// layer hit, because anything behind it is occluded at that point.
    pub fn topmost_at(&self, x: i32, y: i32) -> Option<LayerId> {
        let layers = self.layers.lock();
        layers
            .iter()
            .rev()
            .find(|l| l.visible && l.contains_screen(x, y))
            .map(|l| l.id)
    }

    /// Every layer containing the point, front to back.
    pub fn stack_at(&self, x: i32, y: i32) -> Vec<LayerId> {
        let layers = self.layers.lock();
        layers
            .iter()
            .rev()
            .filter(|l| l.visible && l.contains_screen(x, y))
            .map(|l| l.id)
            .collect()
    }

    /// Mutate a layer's buffer through a closure.
    pub fn with_layer<R>(&self, id: LayerId, f: impl FnOnce(&mut Layer) -> R) -> Result<R, i32> {
        let mut layers = self.layers.lock();
        let layer = layers
            .iter_mut()
            .find(|l| l.id == id)
            .ok_or(errors::NO_OBJECT)?;
        Ok(f(layer))
    }

    /// Composite the stack back-to-front into `target`.
    ///
    /// `target` is a row-major `u32` framebuffer of at least
    /// `stride * height` entries. Only layers marked dirty are copied, and
    /// every layer is left clean: a composite that left layers dirty would
    /// repaint the whole screen on every call, and one that repainted clean
    /// layers would copy megabytes to produce the same image.
    ///
    /// The clip is honoured here rather than assumed: a layer whose clip is
    /// smaller than its buffer must *not* have its unclipped part painted, or
    /// a window that asked to be narrower would paint over its neighbour.
    ///
    /// Returns the union of the rectangles actually touched, so the caller can
    /// decide whether the redraw was worth the copy.
    pub fn composite(&self, target: &mut [u32], stride: u32) -> Result<Rect, i32> {
        if stride == 0 {
            return Err(errors::BAD_RECT);
        }
        let screen = self.screen;
        let mut touched: Option<Rect> = None;
        {
            let mut layers = self.layers.lock();
            for layer in layers.iter_mut() {
                if !layer.dirty || !layer.visible || layer.buffer.is_none() {
                    // The screen layer has no back buffer: its pixels *are*
                    // the target, so it contributes nothing to the copy.
                    layer.dirty = false;
                    continue;
                }
                let screen_rect = layer.screen_rect();
                let Some(draw) = screen_rect.intersect(&screen) else {
                    layer.dirty = false;
                    continue;
                };
                let Some(buffer) = layer.buffer.as_ref() else {
                    unreachable!("checked above");
                };
                for row in 0..draw.height as i32 {
                    let y = draw.y + row;
                    if y < 0 || y >= screen.height as i32 {
                        continue;
                    }
                    // The buffer row that lands on `y`. It differs from the
                    // screen row whenever the layer hangs off an edge, and it
                    // also carries the clip's own y offset — which is why this
                    // is a subtraction of both rather than of the position
                    // alone.
                    let ly = y - layer.position.1 - layer.clip.y;
                    if ly < 0 || ly as usize >= layer.bounds.height as usize {
                        continue;
                    }
                    for col in 0..draw.width as i32 {
                        let x = draw.x + col;
                        if x < 0 || x >= screen.width as i32 {
                            continue;
                        }
                        let lx = x - layer.position.0 - layer.clip.x;
                        if lx < 0 || lx as usize >= layer.bounds.width as usize {
                            continue;
                        }
                        let src = (ly as usize) * (layer.stride as usize) + lx as usize;
                        let dst = (y as usize) * (stride as usize) + x as usize;
                        if src >= buffer.len() || dst >= target.len() {
                            continue;
                        }
                        target[dst] = buffer[src];
                    }
                }
                // The union of everything painted so far, not the
                // intersection: the caller has to repaint the whole area.
                touched = Some(union(touched, draw));
                layer.dirty = false;
            }
        }
        touched.ok_or(errors::NO_OBJECT)
    }

    /// True when any layer is marked dirty.
    pub fn has_dirty(&self) -> bool {
        self.layers.lock().iter().any(|l| l.dirty)
    }

    /// Mark every layer dirty.
    pub fn invalidate_all(&self) {
        for layer in self.layers.lock().iter_mut() {
            layer.dirty = true;
        }
    }

    /// Mark one layer dirty.
    pub fn invalidate(&self, id: LayerId) -> Result<(), i32> {
        let mut layers = self.layers.lock();
        let layer = layers
            .iter_mut()
            .find(|l| l.id == id)
            .ok_or(errors::NO_OBJECT)?;
        layer.dirty = true;
        Ok(())
    }

    /// Convert a colour to the packed `u32` a buffer holds.
    ///
    /// The buffer is `u32` rather than `u8` because the framebuffer's own
    /// byte order is a property of the display mode: compositing in `u32` and
    /// letting the renderer pack the final pixel is what lets the same layer
    /// code work in `Rgb` and in `Bgr`.
    pub fn pack(color: Color) -> u32 {
        ((color.r as u32) << 16) | ((color.g as u32) << 8) | color.b as u32
    }
}

/// The union of two rectangles.
///
/// The *intersection* is what a clip uses; the *union* is what a repaint uses.
/// Conflating them is a real bug: intersecting the touched rectangles makes a
/// composite that touched two separate windows report the empty space between
/// them, and the caller then repaints nothing.
fn union(a: Option<Rect>, b: Rect) -> Rect {
    match a {
        None => b,
        Some(a) => {
            let x0 = a.x.min(b.x);
            let y0 = a.y.min(b.y);
            let x1 = (a.x + a.width as i32).max(b.x + b.width as i32);
            let y1 = (a.y + a.height as i32).max(b.y + b.height as i32);
            Rect::new(x0, y0, (x1 - x0) as u32, (y1 - y0) as u32)
        }
    }
}
#[cfg(test)]
extern crate std;

#[cfg(test)]
mod tests {
    use super::*;

    fn stack() -> LayerStack {
        LayerStack::new(100, 80)
    }

    fn window(s: &LayerStack, x: i32, y: i32, w: u32, h: u32) -> LayerId {
        s.create_buffered_layer(LayerKind::Window, Rect::new(x, y, w, h), 0)
            .expect("window should be created")
    }

    #[test]
    fn a_new_stack_is_empty() {
        let s = stack();
        assert!(s.is_empty());
        assert_eq!(s.screen(), Rect::new(0, 0, 100, 80));
    }

    #[test]
    fn a_layer_gets_a_handle_and_can_be_destroyed() {
        let s = stack();
        let id = window(&s, 0, 0, 10, 10);
        assert!(!id.is_none());
        assert!(s.exists(id));
        assert_eq!(s.len(), 1);
        let layer = s.destroy_layer(id).expect("destroyed");
        assert_eq!(layer.id, id);
        assert!(!s.exists(id));
        assert!(s.destroy_layer(id).is_err());
    }

    #[test]
    fn layer_handles_are_never_reused_while_alive() {
        // A stale window handle naming a different window is the ABA problem,
        // and it is why the id space grows monotonically.
        let s = stack();
        let a = window(&s, 0, 0, 10, 10);
        let b = window(&s, 0, 0, 10, 10);
        assert_ne!(a, b);
    }

    #[test]
    fn a_zero_sized_layer_is_refused() {
        let s = stack();
        assert_eq!(
            s.create_layer(LayerKind::Window, Rect::new(0, 0, 0, 10))
                .unwrap_err(),
            errors::BAD_RECT
        );
        assert_eq!(
            s.create_layer(LayerKind::Window, Rect::new(0, 0, 10, 0))
                .unwrap_err(),
            errors::BAD_RECT
        );
    }

    #[test]
    fn a_new_layers_buffer_starts_filled() {
        // A window shown before it has drawn must not show heap garbage.
        let s = stack();
        let id = s
            .create_buffered_layer(LayerKind::Window, Rect::new(0, 0, 4, 4), 0xDEAD_BEEF)
            .unwrap();
        let layer = s.layer(id).unwrap();
        assert_eq!(layer.pixel_count(), 16);
        assert_eq!(layer.get(0, 0), Some(0xDEAD_BEEF));
        assert_eq!(layer.get(3, 3), Some(0xDEAD_BEEF));
    }

    #[test]
    fn writes_outside_a_layer_are_refused() {
        let s = stack();
        let id = window(&s, 0, 0, 4, 4);
        let mut layer = s.layer(id).unwrap();
        assert!(!layer.put(4, 0, 1), "x == width is outside");
        assert!(!layer.put(0, 4, 1));
        assert!(!layer.put(-1, 0, 1));
        assert!(!layer.put(0, -1, 1));
        assert!(layer.put(3, 3, 1), "the last pixel is inside");
        assert!(layer.put(0, 0, 1), "an overwrite is still inside");
    }

    #[test]
    fn a_layer_without_a_buffer_refuses_writes() {
        let s = stack();
        let id = s
            .create_layer(LayerKind::Window, Rect::new(0, 0, 4, 4))
            .unwrap();
        let mut layer = s.layer(id).unwrap();
        assert!(!layer.put(0, 0, 1));
        assert_eq!(layer.get(0, 0), None);
    }

    #[test]
    fn writing_marks_the_layer_dirty() {
        let s = stack();
        let id = window(&s, 0, 0, 4, 4);
        s.with_layer(id, |l| l.dirty = false).unwrap();
        s.with_layer(id, |l| l.put(0, 0, 1)).unwrap();
        assert!(s.layer(id).unwrap().dirty);
    }

    #[test]
    fn a_clip_larger_than_the_layer_is_clamped() {
        // A clip larger than the bounds would let the layer draw outside
        // itself and corrupt whatever is behind it. The returned clip is in
        // *layer* coordinates, so it starts at the origin.
        let s = stack();
        let id = window(&s, 10, 10, 20, 20);
        let effective = s.set_clip(id, Rect::new(0, 0, 1000, 1000)).unwrap();
        assert_eq!(effective, Rect::new(0, 0, 20, 20));
        assert_eq!(
            s.layer(id).unwrap().screen_rect(),
            Rect::new(10, 10, 20, 20)
        );
    }

    #[test]
    fn a_clip_moves_the_drawable_area_on_screen() {
        // The clip is in layer coordinates, so a clip at (5, 5) inside a
        // window at (10, 10) paints from screen (15, 15).
        let s = stack();
        let id = window(&s, 10, 10, 20, 20);
        let effective = s.set_clip(id, Rect::new(5, 5, 10, 10)).unwrap();
        assert_eq!(effective, Rect::new(5, 5, 10, 10));
        let layer = s.layer(id).unwrap();
        assert_eq!(layer.screen_rect(), Rect::new(15, 15, 10, 10));
        // `to_local` reports coordinates relative to the *clip*, because that
        // is what a program drawing into the clipped window wants to index.
        assert_eq!(layer.to_local(15, 15), Some((0, 0)));
        assert_eq!(layer.to_local(24, 24), Some((9, 9)));
        assert_eq!(layer.to_local(10, 10), None, "the clipped-away corner");
        assert_eq!(layer.to_local(25, 25), None, "past the clip");
    }

    #[test]
    fn composite_honours_a_layer_clip() {
        // A window that asked to be narrower must not paint the part it
        // clipped away, or it would paint over whatever is behind it.
        let s = stack();
        let id = window(&s, 0, 0, 20, 20);
        s.with_layer(id, |l| l.clear(0xFF)).unwrap();
        s.set_clip(id, Rect::new(0, 0, 10, 10)).unwrap();
        let mut fb = alloc::vec![0u32; 100 * 80];
        s.composite(&mut fb, 100).unwrap();
        assert_eq!(fb[5 * 100 + 5], 0xFF, "inside the clip");
        assert_eq!(fb[15 * 100 + 15], 0, "clipped away");
    }

    #[test]
    fn a_clip_disjoint_from_the_layer_becomes_empty() {
        let s = stack();
        let id = window(&s, 10, 10, 20, 20);
        let effective = s.set_clip(id, Rect::new(500, 500, 5, 5)).unwrap();
        assert_eq!(effective, Rect::new(0, 0, 0, 0));
    }

    #[test]
    fn moving_a_layer_returns_the_old_and_new_rectangles() {
        // The caller must repaint the union: without the old rectangle the
        // window's previous pixels stay on screen as a ghost.
        let s = stack();
        let id = window(&s, 0, 0, 10, 10);
        let (old, new) = s.move_layer(id, 20, 30).unwrap();
        assert_eq!(old, Rect::new(0, 0, 10, 10));
        assert_eq!(new, Rect::new(20, 30, 10, 10));
    }

    #[test]
    fn moving_an_unknown_layer_is_an_error() {
        let s = stack();
        assert!(s.move_layer(LayerId(42), 0, 0).is_err());
    }

    #[test]
    fn resizing_preserves_the_content_that_still_fits() {
        // A shrink that dropped the buffer would blank the window without the
        // program ever being told to redraw.
        let s = stack();
        let id = window(&s, 0, 0, 4, 4);
        s.with_layer(id, |l| {
            // (0,0)=A (1,0)=B (2,0)=C (3,0)=D
            // (0,1)=E (1,1)=F (2,1)=G (3,1)=H
            for (i, v) in b"ABCDEFGH".iter().enumerate() {
                l.put((i % 4) as i32, (i / 4) as i32, *v as u32);
            }
        })
        .unwrap();
        let old = s.resize_layer(id, Rect::new(0, 0, 2, 2)).unwrap();
        assert_eq!(
            old,
            Rect::new(0, 0, 4, 4),
            "the old rectangle must come back"
        );
        let layer = s.layer(id).unwrap();
        assert_eq!(layer.bounds, Rect::new(0, 0, 2, 2));
        assert_eq!(layer.get(0, 0), Some(b'A' as u32));
        assert_eq!(layer.get(1, 1), Some(b'F' as u32));
        // The new area is zeroed, not left holding a stale value.
        assert_eq!(layer.get(2, 0), None);
        assert_eq!(layer.pixel_count(), 4);
    }

    #[test]
    fn growing_a_layer_keeps_the_old_content() {
        let s = stack();
        let id = window(&s, 0, 0, 2, 2);
        s.with_layer(id, |l| l.put(1, 1, 0xABCD)).unwrap();
        s.resize_layer(id, Rect::new(0, 0, 6, 6)).unwrap();
        let layer = s.layer(id).unwrap();
        assert_eq!(layer.get(1, 1), Some(0xABCD));
        assert_eq!(layer.get(0, 0), Some(0));
        assert_eq!(layer.pixel_count(), 36);
    }

    #[test]
    fn resizing_a_layer_without_a_buffer_still_works() {
        let s = stack();
        let id = s
            .create_layer(LayerKind::Window, Rect::new(0, 0, 4, 4))
            .unwrap();
        s.resize_layer(id, Rect::new(0, 0, 8, 8)).unwrap();
        let layer = s.layer(id).unwrap();
        assert!(layer.buffer.is_none());
        assert_eq!(layer.bounds, Rect::new(0, 0, 8, 8));
    }

    #[test]
    fn a_new_layer_goes_to_the_front() {
        let s = stack();
        let a = window(&s, 0, 0, 10, 10);
        let b = window(&s, 0, 0, 10, 10);
        assert_eq!(s.order(), alloc::vec![a, b]);
        assert_eq!(s.topmost_at(5, 5), Some(b), "the newest is in front");
    }

    #[test]
    fn bringing_a_layer_to_the_front_puts_it_last() {
        let s = stack();
        let a = window(&s, 0, 0, 10, 10);
        let b = window(&s, 0, 0, 10, 10);
        let c = window(&s, 0, 0, 10, 10);
        s.to_front(a).unwrap();
        assert_eq!(s.order(), alloc::vec![b, c, a]);
        assert_eq!(s.topmost_at(5, 5), Some(a));
    }

    #[test]
    fn sending_a_layer_to_the_back_puts_it_behind_the_others() {
        let s = stack();
        let a = window(&s, 0, 0, 10, 10);
        let b = window(&s, 0, 0, 10, 10);
        let c = window(&s, 0, 0, 10, 10);
        s.to_back(c).unwrap();
        assert_eq!(s.order(), alloc::vec![c, a, b]);
    }

    #[test]
    fn sending_to_the_back_stops_above_the_screen_layer() {
        // The screen layer is the backdrop; a layer behind it would be
        // invisible, so the back of the stack is just above it.
        let s = stack();
        let screen = s.create_screen_layer().unwrap();
        let a = window(&s, 0, 0, 10, 10);
        let b = window(&s, 0, 0, 10, 10);
        let c = window(&s, 0, 0, 10, 10);
        assert_eq!(s.order(), alloc::vec![screen, a, b, c]);
        s.to_back(c).unwrap();
        assert_eq!(s.order(), alloc::vec![screen, c, a, b]);
    }

    #[test]
    fn a_screen_layer_has_no_back_buffer() {
        // The screen layer's pixels *are* the target, so it contributes nothing
        // to the copy and must not be treated as an unbacked window.
        let s = stack();
        let screen = s.create_screen_layer().unwrap();
        let layer = s.layer(screen).unwrap();
        assert!(layer.buffer.is_none());
        let mut fb = alloc::vec![0u32; 100 * 80];
        assert!(s.composite(&mut fb, 100).is_err());
    }

    #[test]
    fn sending_to_the_back_on_a_single_layer_stack_is_a_no_op() {
        // With only one layer, index one does not exist; clamping must not
        // panic or corrupt the order.
        let s = stack();
        let a = window(&s, 0, 0, 10, 10);
        s.to_back(a).unwrap();
        assert_eq!(s.order(), alloc::vec![a]);
    }

    #[test]
    fn reordering_marks_everything_dirty() {
        // A layer that came from behind now covers whatever was in front of it,
        // so the layers it now hides are dirty too.
        let s = stack();
        let a = window(&s, 0, 0, 10, 10);
        let b = window(&s, 0, 0, 10, 10);
        s.with_layer(a, |l| l.dirty = false).unwrap();
        s.with_layer(b, |l| l.dirty = false).unwrap();
        s.to_front(a).unwrap();
        assert!(s.has_dirty());
    }

    #[test]
    fn hiding_a_layer_takes_it_out_of_the_hit_test() {
        let s = stack();
        let a = window(&s, 0, 0, 20, 20);
        assert_eq!(s.topmost_at(5, 5), Some(a));
        s.set_visible(a, false).unwrap();
        assert_eq!(s.topmost_at(5, 5), None);
    }

    #[test]
    fn hit_testing_returns_the_topmost_layer() {
        let s = stack();
        let back = window(&s, 0, 0, 20, 20);
        let front = window(&s, 10, 10, 20, 20);
        assert_eq!(
            s.topmost_at(5, 5),
            Some(back),
            "only the back layer is there"
        );
        assert_eq!(s.topmost_at(15, 15), Some(front), "both overlap");
        assert_eq!(s.topmost_at(50, 50), None, "outside both");
        assert_eq!(s.stack_at(15, 15), alloc::vec![front, back]);
    }

    #[test]
    fn a_screen_coordinate_maps_into_layer_coordinates() {
        let s = stack();
        let id = window(&s, 10, 20, 30, 30);
        let layer = s.layer(id).unwrap();
        assert_eq!(layer.to_local(10, 20), Some((0, 0)));
        assert_eq!(layer.to_local(39, 49), Some((29, 29)));
        assert_eq!(layer.to_local(9, 20), None);
        assert_eq!(layer.to_local(40, 20), None);
    }

    #[test]
    fn a_layer_partly_off_screen_composites_only_the_visible_part() {
        let s = stack();
        let id = window(&s, -5, -5, 20, 20);
        s.with_layer(id, |l| l.clear(0xFF)).unwrap();
        let mut fb = alloc::vec![0u32; 100 * 80];
        s.composite(&mut fb, 100).unwrap();
        // The visible quarter is painted.
        assert_eq!(fb[0], 0xFF, "top-left of the screen");
        assert_eq!(fb[14 * 100 + 14], 0xFF, "the last visible pixel");
        // Nothing outside it was touched.
        assert_eq!(fb[15 * 100 + 15], 0);
    }

    #[test]
    fn a_layer_entirely_off_screen_composites_nothing() {
        let s = stack();
        let id = window(&s, 500, 500, 20, 20);
        s.with_layer(id, |l| l.clear(0xFF)).unwrap();
        let mut fb = alloc::vec![0u32; 100 * 80];
        assert!(s.composite(&mut fb, 100).is_err(), "nothing was touched");
        for v in fb.iter() {
            assert_eq!(*v, 0);
        }
    }

    #[test]
    fn layers_composite_back_to_front() {
        // The whole point of z-order: the front layer's pixels win.
        let s = stack();
        let back = window(&s, 0, 0, 50, 50);
        let front = window(&s, 0, 0, 50, 50);
        s.with_layer(back, |l| l.clear(0xAA)).unwrap();
        s.with_layer(front, |l| l.clear(0xBB)).unwrap();
        let mut fb = alloc::vec![0u32; 100 * 80];
        s.composite(&mut fb, 100).unwrap();
        assert_eq!(fb[0], 0xBB, "the front layer is on top");
        assert_eq!(fb[60 * 100 + 60], 0, "outside both");
    }

    #[test]
    fn composite_clears_the_dirty_flag() {
        // A composite that leaves layers dirty would repaint the whole screen
        // on every subsequent call.
        let s = stack();
        let id = window(&s, 0, 0, 10, 10);
        let mut fb = alloc::vec![0u32; 100 * 80];
        s.composite(&mut fb, 100).unwrap();
        assert!(!s.layer(id).unwrap().dirty);
        assert!(!s.has_dirty());
        // And a second composite with nothing dirty touches nothing.
        assert!(s.composite(&mut fb, 100).is_err());
    }

    #[test]
    fn composite_honours_the_target_stride() {
        // The framebuffer's stride is not its width: a target padded to 128
        // columns must have its padding left alone.
        let s = stack();
        let id = window(&s, 0, 0, 10, 10);
        s.with_layer(id, |l| l.clear(0xFF)).unwrap();
        let mut fb = alloc::vec![0u32; 128 * 80];
        s.composite(&mut fb, 128).unwrap();
        assert_eq!(fb[0], 0xFF);
        assert_eq!(fb[10], 0, "the padding column is untouched");
        assert_eq!(fb[128 * 3], 0xFF, "row three");
        assert_eq!(fb[128 * 3 + 10], 0);
    }

    #[test]
    fn a_zero_stride_target_is_refused() {
        let s = stack();
        let mut fb = alloc::vec![0u32; 16];
        assert_eq!(s.composite(&mut fb, 0).unwrap_err(), errors::BAD_RECT);
    }

    #[test]
    fn an_offscreen_layer_is_created_hidden() {
        // An offscreen layer is one the program renders into without showing;
        // showing it would be a bug, so it starts invisible.
        let s = stack();
        let id = s
            .create_buffered_layer(LayerKind::Offscreen, Rect::new(0, 0, 10, 10), 0)
            .unwrap();
        assert!(!s.layer(id).unwrap().visible);
        assert_eq!(s.topmost_at(5, 5), None);
    }

    #[test]
    fn index_of_reports_the_z_position() {
        let s = stack();
        let a = window(&s, 0, 0, 10, 10);
        let b = window(&s, 0, 0, 10, 10);
        assert_eq!(s.index_of(a), Some(0));
        assert_eq!(s.index_of(b), Some(1));
        assert_eq!(s.index_of(LayerId(99)), None);
    }

    #[test]
    fn invalidate_marks_one_layer() {
        let s = stack();
        let a = window(&s, 0, 0, 10, 10);
        let b = window(&s, 0, 0, 10, 10);
        s.invalidate_all();
        s.with_layer(a, |l| l.dirty = false).unwrap();
        s.with_layer(b, |l| l.dirty = false).unwrap();
        s.invalidate(a).unwrap();
        assert!(s.layer(a).unwrap().dirty);
        assert!(!s.layer(b).unwrap().dirty);
        assert!(s.invalidate(LayerId(99)).is_err());
    }

    #[test]
    fn packing_a_colour_uses_0x00rrggbb_ordering() {
        assert_eq!(LayerStack::pack(Color::rgb(0x11, 0x22, 0x33)), 0x0011_2233);
    }

    #[test]
    fn a_layer_is_debuggable() {
        let s = stack();
        let id = window(&s, 1, 2, 3, 4);
        let text = alloc::format!("{:?}", s.layer(id).unwrap());
        assert!(text.contains("Window"), "{text}");
        assert!(alloc::format!("{:?}", s).contains("LayerStack"));
    }
}
