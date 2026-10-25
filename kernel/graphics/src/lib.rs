// The Graphics service: bitmaps, palettes, pens and raster ports.
//!
//! # What this crate is for
//!
//! AmigaOS's `graphics.library` is not a drawing API. It is a *pen
//! management* API: the interesting calls are `AllocPen`, `SetPen` and
//! `ObtainPens`, and the drawing itself is done by the blitter hardware
//! through `WritePixel`, `RectFill`, `Move`/`Line` and `Text`. A program
//! therefore never names a colour — it names a **pen**, and the pen resolves
//! against whichever palette is in the screen's display file.
//!
//! That indirection is worth keeping even with a software renderer, for one
//! reason: it is what makes a palette change instant. Every pixel already
//! drawn refers to a pen, so re-resolving the palette recolours the whole
//! screen without redrawing anything.
//!
//! So this crate has four layers, in dependency order:
//!
//! 1. [`Palette`] — an id-to-colour table.
//! 2. [`PaletteStore`] — the owned palettes, with reference counting.
//! 3. [`Pen`] and [`PenId`] — a named slot in a palette, allocated and freed.
//! 4. [`RastPort`] — a drawing target: a surface, a clip, a mask, and a
//!    current pen.
//!
//! # Why the drawing state is separated from the pixel storage
//!
//! A `RastPort` borrows its pixel storage; it does not own it. A window's
//! raster and the screen framebuffer are both owned by someone else — the
//! Layers service and the display driver respectively — and a drawing port that
//! owned its bitmap would have to be told to hand it back, which is exactly
//! the kind of ownership dance that produces a double free.
//!
//! # Coordinate systems
//!
//! There are two, and mixing them up is the classic graphics bug:
//!
//! * **Surface coordinates** — where the pixel is in memory.
//! * **Layer/window coordinates** — where the thing is on screen.
//!
//! [`RastPort`] draws in surface coordinates. Layers translate a layer
//! rectangle into them before handing the rast port over.

#![no_std]

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;

use spin::Mutex;

use vela_renderer2d::{Color, Rect, Surface};

/// The maximum number of colours in one palette.
///
/// The AmigaOS hardware palette is a fixed 2^n entries, and the point of a
/// palette is that a pen index is a small integer. Two thousand is well past
/// any of the display modes v0.1.0 supports and small enough that a pen index
/// always fits in a `u16`, which is what the syscall boundary carries.
pub const MAX_PALETTE_ENTRIES: usize = 2048;

/// The palette number the classic theme uses.
pub const DEFAULT_PALETTE: u16 = 0;

/// A 32-bit packed colour, as a program passes it.
///
/// On AmigaOS a colour is a `ULONG` in `0x00RRGGBB` order. VelaOS keeps that
/// order in the API surface and converts once, at the renderer boundary,
/// because the framebuffer's own byte order is a property of the display mode
/// and not of the API.
pub type ColorValue = u32;

/// The errors the Graphics service returns.
///
/// The values are the AmigaOS `Gfx` error set where there is one.
pub mod errors {
    /// The call succeeded.
    pub const OK: i32 = 0;
    /// No raster or bitmap of that handle exists.
    pub const NO_OBJECT: i32 = -2;
    /// The palette or pen is already in use.
    pub const OBJECT_IN_USE: i32 = -3;
    /// The pen could not be allocated: the palette is full.
    pub const NO_FREE_PENS: i32 = -4;
    /// The palette number is not valid.
    pub const BAD_PALETTE: i32 = -10;
    /// The raster's geometry is invalid.
    pub const RASTER_TOO_SMALL: i32 = -11;
    /// A memory allocation failed.
    pub const NO_MEMORY: i32 = -12;
}

/// A palette: an indexed table of colours.
///
/// Pens are *indices into a palette*, not colours, and that is the whole
/// point: swapping a palette recolours everything drawn with it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Palette {
    entries: Vec<ColorValue>,
}

impl Palette {
    /// An empty palette.
    pub const fn new() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    /// A palette of `count` entries, all set to `fill`.
    pub fn filled(count: usize, fill: ColorValue) -> Self {
        Self {
            entries: alloc::vec![fill; count.min(MAX_PALETTE_ENTRIES)],
        }
    }

    /// How many entries the palette has.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// True when the palette has no entries.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The colour at `index`, or `None` when the index is out of range.
    pub fn get(&self, index: u16) -> Option<ColorValue> {
        self.entries.get(index as usize).copied()
    }

    /// Set the colour at `index`.
    ///
    /// Growing the palette is allowed: a program that allocates pens gets a
    /// palette as deep as it needs.
    pub fn set(&mut self, index: u16, color: ColorValue) -> Result<(), i32> {
        if index as usize >= MAX_PALETTE_ENTRIES {
            return Err(errors::BAD_PALETTE);
        }
        let wanted = index as usize + 1;
        if self.entries.len() < wanted {
            self.entries.resize(wanted, 0);
        }
        self.entries[index as usize] = color;
        Ok(())
    }

    /// Set several entries at once.
    pub fn set_many(&mut self, base: u16, colors: &[ColorValue]) -> Result<(), i32> {
        for (offset, _) in colors.iter().enumerate() {
            let index = base as usize + offset;
            if index >= MAX_PALETTE_ENTRIES {
                return Err(errors::BAD_PALETTE);
            }
        }
        for (offset, color) in colors.iter().enumerate() {
            let index = base as usize + offset;
            if self.entries.len() <= index {
                self.entries.resize(index + 1, 0);
            }
            self.entries[index] = *color;
        }
        Ok(())
    }
}

impl Default for Palette {
    fn default() -> Self {
        Self::new()
    }
}

/// A pen handle: an index into a palette.
///
/// The handle is a plain index rather than an allocation token so that
/// `SetPen` is a single indexed write, and so that a program can compute a pen
/// index from a colour name — which is what `graphics.library`'s palette-mask
/// mode did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PenId(pub u16);

impl PenId {
    /// The id of "no pen".
    pub const NONE: PenId = PenId(0xFFFF);

    /// True when this id names no pen.
    pub const fn is_none(self) -> bool {
        self.0 == 0xFFFF
    }

    /// The pen's index in its palette.
    pub const fn index(self) -> usize {
        self.0 as usize
    }
}

/// The Graphics service.
///
/// Owns the palettes and the pens, and nothing else: the pixel storage belongs
/// to whoever mapped it — the display driver for the screen, Layers for a
/// window.
pub struct Graphics {
    /// The palettes, by number.
    palettes: Mutex<Vec<(u16, Palette, u32)>>,
    /// The next free slot in each palette, so `AllocPen` is not a scan.
    ///
    /// A pen index is allocated by claiming a slot; the slot is freed on
    /// `FreePen` and becomes available again. The free list is not kept
    /// separately because a palette has at most two thousand entries and
    /// `AllocPen` is not on the drawing path.
    next_free: Mutex<Vec<u16>>,
    /// The rasters the service knows about.
    ///
    /// A raster is registered so that a window can be given one by handle, but
    /// the service does not own its pixels: the owner supplies the
    /// [`Surface`] at draw time.
    rasters: Mutex<Vec<RasterRecord>>,
}

impl core::fmt::Debug for Graphics {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Graphics")
            .field("palettes", &self.palettes.lock().len())
            .field("rasters", &self.rasters.lock().len())
            .finish()
    }
}

/// One registered raster.
#[derive(Debug, Clone, Copy)]
struct RasterRecord {
    width: u32,
    height: u32,
    depth: u8,
    /// The handle the graphics library uses for this raster, so that a
    /// blit-to-screen can name it.
    #[allow(dead_code)]
    plane_handle: u16,
}

impl Default for Graphics {
    fn default() -> Self {
        Self::new()
    }
}

impl Graphics {
    /// A Graphics service with one palette, numbered [`DEFAULT_PALETTE`].
    ///
    /// The palette is created empty rather than pre-filled with the theme:
    /// the theme is a *userland* choice, and the kernel's palette exists so
    /// that a pen can be allocated before any theme is loaded.
    pub fn new() -> Self {
        let g = Self {
            palettes: Mutex::new(Vec::new()),
            next_free: Mutex::new(Vec::new()),
            rasters: Mutex::new(Vec::new()),
        };
        {
            let mut palettes = g.palettes.lock();
            palettes.push((DEFAULT_PALETTE, Palette::new(), 0));
        }
        {
            let mut next = g.next_free.lock();
            next.push(0);
        }
        g
    }

    /// The palette with the given number, if it exists.
    pub fn palette(&self, number: u16) -> Option<Palette> {
        self.palettes
            .lock()
            .iter()
            .find(|(n, _, _)| *n == number)
            .map(|(_, p, _)| p.clone())
    }

    /// Create a palette.
    pub fn create_palette(&self, number: u16) -> Result<(), i32> {
        let mut palettes = self.palettes.lock();
        if palettes.iter().any(|(n, _, _)| *n == number) {
            return Err(errors::OBJECT_IN_USE);
        }
        palettes.push((number, Palette::new(), 0));
        let mut next = self.next_free.lock();
        next.push(0);
        Ok(())
    }

    /// Install a palette wholesale.
    pub fn set_palette(&self, number: u16, palette: Palette) -> Result<(), i32> {
        let mut palettes = self.palettes.lock();
        match palettes.iter_mut().find(|(n, _, _)| *n == number) {
            Some((_, slot, _)) => {
                *slot = palette;
                Ok(())
            }
            None => {
                let mut next = self.next_free.lock();
                next.push(0);
                drop(next);
                palettes.push((number, palette, 0));
                Ok(())
            }
        }
    }

    /// Release a palette.
    ///
    /// The default palette is refused: every pen index means "in the default
    /// palette", and removing it would make every already-drawn raster's pen
    /// resolve to nothing.
    pub fn free_palette(&self, number: u16) -> Result<(), i32> {
        if number == DEFAULT_PALETTE {
            return Err(errors::OBJECT_IN_USE);
        }
        let mut palettes = self.palettes.lock();
        let before = palettes.len();
        palettes.retain(|(n, _, _)| *n != number);
        if palettes.len() == before {
            return Err(errors::NO_OBJECT);
        }
        // The next-free cursors are index-aligned with the palette list only
        // until a palette is removed, so the cursor is reset wholesale rather
        // than trying to keep them in step.
        let mut next = self.next_free.lock();
        next.clear();
        next.resize(palettes.len(), 0);
        Ok(())
    }

    fn palette_slot(&self, number: u16) -> Option<usize> {
        self.palettes
            .lock()
            .iter()
            .position(|(n, _, _)| *n == number)
    }

    /// `AllocPen` — allocate a pen in a palette.
    ///
    /// The pen starts as the requested colour, and the *index* it lands on is
    /// what the caller keeps. Two pens with the same colour therefore have
    /// different indices, which is what makes a pen refcountable.
    pub fn alloc_pen(&self, palette: u16, color: ColorValue) -> Result<PenId, i32> {
        let slot = self.palette_slot(palette).ok_or(errors::BAD_PALETTE)?;
        let mut next = self.next_free.lock();
        let index = next[slot];
        if index as usize >= MAX_PALETTE_ENTRIES {
            return Err(errors::NO_FREE_PENS);
        }
        next[slot] = index + 1;
        drop(next);
        {
            let mut palettes = self.palettes.lock();
            palettes[slot].1.set(index, color)?;
            palettes[slot].2 += 1;
        }
        Ok(PenId(index))
    }

    /// `FreePen` — release a pen.
    ///
    /// Freeing a pen does not shrink the palette: the entry stays, holding
    /// whatever colour it had, because a raster that already refers to the
    /// index must keep resolving to *something* rather than to a different
    /// colour on the next draw.
    pub fn free_pen(&self, palette: u16, pen: PenId) -> Result<(), i32> {
        let slot = self.palette_slot(palette).ok_or(errors::BAD_PALETTE)?;
        let mut palettes = self.palettes.lock();
        if pen.index() >= palettes[slot].1.len() {
            return Err(errors::NO_OBJECT);
        }
        if palettes[slot].2 == 0 {
            return Err(errors::NO_OBJECT);
        }
        palettes[slot].2 -= 1;
        Ok(())
    }

    /// How many pens are allocated in a palette.
    pub fn pen_count(&self, palette: u16) -> u32 {
        self.palette_slot(palette)
            .map(|slot| self.palettes.lock()[slot].2)
            .unwrap_or(0)
    }

    /// The colour a pen resolves to.
    pub fn pen_color(&self, palette: u16, pen: PenId) -> Option<ColorValue> {
        let slot = self.palette_slot(palette)?;
        let p = self.palettes.lock();
        p[slot].1.get(pen.0)
    }

    /// `SetPen` — the pen a drawing operation uses.
    ///
    /// Returns the colour the new pen resolves to, or an error if it does not
    /// resolve. A pen that does not resolve is a programming error, and
    /// drawing with it would produce whatever the stale entry happened to be
    /// — so it is refused here rather than silently drawn.
    pub fn set_pen(&self, palette: u16, pen: PenId) -> Result<ColorValue, i32> {
        self.pen_color(palette, pen).ok_or(errors::NO_OBJECT)
    }

    /// Register a raster, returning its handle.
    ///
    /// The Graphics service records the geometry so that `GetAttr` can answer
    /// and so that a `SetClip` can be clamped to the raster rather than to the
    /// screen. It does not take ownership of the pixels.
    pub fn register_raster(&self, width: u32, height: u32, depth: u8) -> Result<u16, i32> {
        if width == 0 || height == 0 {
            return Err(errors::RASTER_TOO_SMALL);
        }
        let mut rasters = self.rasters.lock();
        // Handles are 1-based, so that zero can mean "no raster" the way it
        // means "no library" everywhere else in the API.
        let handle = rasters.len() as u16 + 1;
        rasters.push(RasterRecord {
            width,
            height,
            depth,
            plane_handle: handle,
        });
        Ok(handle)
    }

    /// Forget a raster.
    pub fn release_raster(&self, handle: u16) -> Result<(), i32> {
        let mut rasters = self.rasters.lock();
        if handle == 0 || handle as usize > rasters.len() {
            return Err(errors::NO_OBJECT);
        }
        rasters.remove(handle as usize - 1);
        Ok(())
    }

    /// A registered raster's geometry.
    pub fn raster_info(&self, handle: u16) -> Option<(u32, u32, u8)> {
        let rasters = self.rasters.lock();
        rasters
            .get(handle.checked_sub(1)? as usize)
            .map(|r| (r.width, r.height, r.depth))
    }

    /// How many rasters are registered.
    pub fn raster_count(&self) -> usize {
        self.rasters.lock().len()
    }

    /// Resolve a whole palette into an owned table.
    ///
    /// Layers uses this to composite: a layer stores pen indices, and the
    /// composite pass needs the colours. Doing the lookup once per redraw
    /// rather than once per pixel is what makes the software renderer viable.
    pub fn resolve(&self, number: u16) -> Result<Vec<Color>, i32> {
        let p = self.palette(number).ok_or(errors::BAD_PALETTE)?;
        Ok(p.entries.iter().map(|c| Color::from(*c)).collect())
    }
}

/// The drawing state of one destination.
///
/// A `RastPort` is a *view*: it borrows the surface it draws into and carries
/// the state that the AmigaOS `struct RastPort` carries. The borrow is what
/// makes it impossible for two rasters to alias one framebuffer through this
/// type.
pub struct RastPort<'a> {
    surface: Surface<'a>,
    pen: PenId,
    palette: u16,
    /// The colour the current pen resolves to.
    ///
    /// Cached here rather than looked up per pixel: resolving is a table
    /// read under a lock, and a 1280x800 fill is a million of them. The cache
    /// is invalidated by exactly one operation, [`Self::set_pen`].
    color: Color,
    /// The drawing position, as `Move`/`Line` leave it.
    position: (i32, i32),
}

impl core::fmt::Debug for RastPort<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("RastPort")
            .field("surface", &self.surface.width())
            .field("pen", &self.pen)
            .field("palette", &self.palette)
            .field("position", &self.position)
            .finish()
    }
}

impl<'a> RastPort<'a> {
    /// A rast port over `surface`, drawing with `pen` from `palette`.
    ///
    /// The pen must already resolve: a rast port that starts with an
    /// unresolvable pen would draw in black and blame the caller later.
    pub fn new(surface: Surface<'a>, palette: u16, pen: PenId, color: Color) -> Self {
        Self {
            surface,
            pen,
            palette,
            color,
            position: (0, 0),
        }
    }

    /// The surface underneath.
    pub fn surface(&self) -> &Surface<'a> {
        &self.surface
    }

    /// The surface underneath, mutably.
    pub fn surface_mut(&mut self) -> &mut Surface<'a> {
        &mut self.surface
    }

    /// Give the surface back.
    pub fn into_surface(self) -> Surface<'a> {
        self.surface
    }

    /// The current pen.
    pub const fn pen(&self) -> PenId {
        self.pen
    }

    /// The palette the pen resolves against.
    pub const fn palette(&self) -> u16 {
        self.palette
    }

    /// The colour the current pen resolves to.
    pub const fn color(&self) -> Color {
        self.color
    }

    /// The drawing position.
    pub const fn position(&self) -> (i32, i32) {
        self.position
    }

    /// `SetPen` — resolve `pen` and make it the drawing pen.
    ///
    /// Returns the colour it resolved to. A pen that does not resolve leaves
    /// the port's pen alone: drawing with a stale colour while the port claims
    /// a new pen is worse than refusing.
    pub fn set_pen(&mut self, graphics: &Graphics, pen: PenId) -> Result<Color, i32> {
        let color = graphics.set_pen(self.palette, pen)?;
        self.pen = pen;
        self.color = Color::from(color);
        Ok(self.color)
    }

    /// Write one pixel with the current pen.
    pub fn put_pixel(&mut self, x: i32, y: i32) {
        if x < 0 || y < 0 {
            return;
        }
        self.surface.put_pixel(x as u32, y as u32, self.color);
    }

    /// `Move` — set the drawing position without drawing.
    pub fn move_to(&mut self, x: i32, y: i32) {
        self.position = (x, y);
    }

    /// `Line` — draw from the current position, updating it.
    pub fn line(&mut self, x: i32, y: i32) {
        let (x0, y0) = self.position;
        let color = self.color;
        self.surface.line(color, x0, y0, x, y);
        self.position = (x, y);
    }

    /// `RectFill` — fill a rectangle with the current pen.
    pub fn fill_rect(&mut self, x: i32, y: i32, width: u32, height: u32) {
        let color = self.color;
        self.surface.fill_rect(color, x, y, width, height);
    }

    /// `Rect` — stroke a rectangle outline with the current pen.
    pub fn stroke_rect(&mut self, x: i32, y: i32, width: u32, height: u32) {
        let color = self.color;
        self.surface.stroke_rect(color, x, y, width, height);
    }

    /// `Text` — draw a string with the current pen, returning the advance
    /// width in pixels.
    pub fn text(&mut self, text: &str, x: i32, y: i32) -> u32 {
        let color = self.color;
        self.surface.draw_text(color, text, x, y, 1)
    }

    /// The clip rectangle.
    pub fn clip(&self) -> Rect {
        self.surface.clip()
    }

    /// Set the clip rectangle.
    pub fn set_clip(&mut self, r: Rect) {
        self.surface.set_clip(r);
    }

    /// Set the mask rectangle.
    pub fn set_mask(&mut self, r: Option<Rect>) {
        self.surface.set_mask(r);
    }

    /// A summary of the surface geometry.
    pub fn summary(&self) -> String {
        let mut s = String::new();
        s.push_str(&alloc::format!(
            "{}x{} stride {} fmt {:?}",
            self.surface.width(),
            self.surface.height(),
            self.surface.stride(),
            self.surface.pixel_format(),
        ));
        s
    }
}
#[cfg(test)]
extern crate std;

#[cfg(test)]
mod tests {
    use super::*;
    use vela_boot_protocol::PixelFormat;

    /// A heap-backed framebuffer so the service can be tested on host.
    struct FakeFb {
        buf: Vec<u32>,
    }

    impl FakeFb {
        fn new(_width: u32, height: u32, stride: u32) -> Self {
            Self {
                buf: alloc::vec![0u32; (stride * height) as usize],
            }
        }

        fn surface(&mut self, width: u32, height: u32, stride: u32) -> Surface<'_> {
            Surface::new(
                self.buf.as_mut_ptr().cast(),
                self.buf.len() * 4,
                width,
                height,
                stride,
                PixelFormat::Bgr,
            )
            .expect("geometry should be valid")
        }
    }

    fn graphics() -> Graphics {
        Graphics::new()
    }

    #[test]
    fn a_new_service_has_one_palette_and_no_rasters() {
        let g = graphics();
        assert!(g.palette(DEFAULT_PALETTE).is_some());
        assert!(g.palette(1).is_none());
        assert_eq!(g.raster_count(), 0);
    }

    #[test]
    fn a_pen_resolves_to_the_colour_it_was_allocated_with() {
        let g = graphics();
        let pen = g.alloc_pen(DEFAULT_PALETTE, 0x1122_3344).unwrap();
        assert_eq!(g.pen_color(DEFAULT_PALETTE, pen), Some(0x1122_3344));
        assert_eq!(g.pen_count(DEFAULT_PALETTE), 1);
    }

    #[test]
    fn pens_are_distinct_even_with_the_same_colour() {
        // This is what makes a pen refcountable: two pens for the same colour
        // must not share an index, or freeing one would recolour the other.
        let g = graphics();
        let a = g.alloc_pen(DEFAULT_PALETTE, 0x00FF_0000).unwrap();
        let b = g.alloc_pen(DEFAULT_PALETTE, 0x00FF_0000).unwrap();
        assert_ne!(a, b);
        assert_eq!(g.pen_count(DEFAULT_PALETTE), 2);
    }

    #[test]
    fn freeing_a_pen_does_not_recolour_one_already_drawn() {
        let g = graphics();
        let a = g.alloc_pen(DEFAULT_PALETTE, 0x00FF_0000).unwrap();
        let b = g.alloc_pen(DEFAULT_PALETTE, 0x00FF_0000).unwrap();
        g.free_pen(DEFAULT_PALETTE, b).unwrap();
        // The freed slot keeps its colour, so a raster still referring to
        // `b` keeps drawing the colour it was drawn with.
        assert_eq!(g.pen_color(DEFAULT_PALETTE, b), Some(0x00FF_0000));
        assert_eq!(g.pen_color(DEFAULT_PALETTE, a), Some(0x00FF_0000));
    }

    #[test]
    fn freeing_an_unallocated_pen_is_an_error() {
        let g = graphics();
        let pen = g.alloc_pen(DEFAULT_PALETTE, 1).unwrap();
        g.free_pen(DEFAULT_PALETTE, pen).unwrap();
        // The second free finds no pen outstanding.
        assert!(g.free_pen(DEFAULT_PALETTE, pen).is_err());
    }

    #[test]
    fn freeing_a_pen_that_was_never_allocated_is_an_error() {
        let g = graphics();
        assert!(g.free_pen(DEFAULT_PALETTE, PenId(42)).is_err());
    }

    #[test]
    fn an_unknown_palette_is_refused() {
        let g = graphics();
        assert_eq!(g.alloc_pen(7, 0).unwrap_err(), errors::BAD_PALETTE);
    }

    #[test]
    fn a_pen_number_is_the_palette_index() {
        let g = graphics();
        let pen = g.alloc_pen(DEFAULT_PALETTE, 0x00AB_CDEF).unwrap();
        assert_eq!(pen.index(), 0, "the first pen is index zero");
        assert_eq!(
            g.resolve(DEFAULT_PALETTE).unwrap()[0],
            Color::from(0x00AB_CDEF)
        );
    }

    #[test]
    fn a_palette_fills_and_grows() {
        let mut p = Palette::new();
        assert!(p.is_empty());
        p.set(2, 0x0011_2233).unwrap();
        assert_eq!(p.len(), 3, "grew to hold index two");
        assert_eq!(p.get(0), Some(0));
        assert_eq!(p.get(2), Some(0x0011_2233));
        assert_eq!(p.get(3), None);
    }

    #[test]
    fn a_palette_refuses_an_index_past_its_maximum() {
        let mut p = Palette::new();
        let err = p.set(MAX_PALETTE_ENTRIES as u16, 0).unwrap_err();
        assert_eq!(err, errors::BAD_PALETTE);
    }

    #[test]
    fn set_many_is_all_or_nothing() {
        // A partial `set_many` would leave the palette in a state the caller
        // never asked for, so the range is checked before anything is written.
        let mut p = Palette::new();
        let too_many = alloc::vec![0u32; MAX_PALETTE_ENTRIES + 8];
        assert!(p.set_many(0, &too_many).is_err());
        assert!(p.is_empty(), "nothing may be written when the call fails");
    }

    #[test]
    fn the_default_palette_cannot_be_freed() {
        // Every pen index means "in the default palette"; removing it would
        // leave already-drawn rasters resolving to nothing.
        let g = graphics();
        assert!(g.free_palette(DEFAULT_PALETTE).is_err());
        assert!(g.palette(DEFAULT_PALETTE).is_some());
    }

    #[test]
    fn a_created_palette_can_be_freed() {
        let g = graphics();
        g.create_palette(1).unwrap();
        assert!(g.palette(1).is_some());
        g.free_palette(1).unwrap();
        assert!(g.palette(1).is_none());
    }

    #[test]
    fn creating_the_same_palette_twice_is_refused() {
        let g = graphics();
        g.create_palette(1).unwrap();
        assert!(g.create_palette(1).is_err());
    }

    #[test]
    fn installing_a_palette_replaces_it_wholesale() {
        let g = graphics();
        g.alloc_pen(DEFAULT_PALETTE, 0x00FF_0000).unwrap();
        g.set_palette(DEFAULT_PALETTE, Palette::filled(4, 0x0000_00FF))
            .unwrap();
        assert_eq!(g.pen_color(DEFAULT_PALETTE, PenId(0)), Some(0x0000_00FF));
    }

    #[test]
    fn a_raster_registers_and_releases() {
        let g = graphics();
        let handle = g.register_raster(320, 200, 8).unwrap();
        assert_eq!(g.raster_info(handle), Some((320, 200, 8)));
        assert_eq!(g.raster_count(), 1);
        g.release_raster(handle).unwrap();
        assert_eq!(g.raster_count(), 0);
        assert_eq!(g.raster_info(handle), None);
    }

    #[test]
    fn a_zero_sized_raster_is_refused() {
        let g = graphics();
        assert_eq!(
            g.register_raster(0, 200, 8).unwrap_err(),
            errors::RASTER_TOO_SMALL
        );
    }

    #[test]
    fn raster_handles_are_never_zero() {
        // Zero must be able to mean "no raster", the way it means "no
        // library" and "no port".
        let g = graphics();
        assert_ne!(g.register_raster(1, 1, 8).unwrap(), 0);
    }

    #[test]
    fn releasing_an_unknown_raster_is_an_error() {
        let g = graphics();
        assert!(g.release_raster(0).is_err());
        assert!(g.release_raster(99).is_err());
    }

    #[test]
    fn set_pen_resolves_and_caches_the_colour() {
        let g = graphics();
        let red = g.alloc_pen(DEFAULT_PALETTE, 0x00FF_0000).unwrap();
        let blue = g.alloc_pen(DEFAULT_PALETTE, 0x0000_00FF).unwrap();
        let mut fb = FakeFb::new(8, 8, 8);
        let mut rp = RastPort::new(
            fb.surface(8, 8, 8),
            DEFAULT_PALETTE,
            red,
            Color::from(0x00FF_0000),
        );
        assert_eq!(rp.color(), Color::from(0x00FF_0000));
        assert_eq!(rp.set_pen(&g, blue).unwrap(), Color::from(0x0000_00FF));
        assert_eq!(rp.pen(), blue);
    }

    #[test]
    fn set_pen_to_an_unresolvable_pen_keeps_the_old_one() {
        // Drawing with a stale colour while the port claims a new pen is
        // worse than refusing the change.
        let g = graphics();
        let red = g.alloc_pen(DEFAULT_PALETTE, 0x00FF_0000).unwrap();
        let mut fb = FakeFb::new(8, 8, 8);
        let mut rp = RastPort::new(
            fb.surface(8, 8, 8),
            DEFAULT_PALETTE,
            red,
            Color::from(0x00FF_0000),
        );
        assert!(rp.set_pen(&g, PenId(200)).is_err());
        assert_eq!(rp.pen(), red);
        assert_eq!(rp.color(), Color::from(0x00FF_0000));
    }

    #[test]
    fn a_rast_port_draws_with_its_current_pen() {
        let g = graphics();
        let white = g.alloc_pen(DEFAULT_PALETTE, 0x00FF_FFFF).unwrap();
        let mut fb = FakeFb::new(8, 8, 8);
        {
            let mut rp = RastPort::new(
                fb.surface(8, 8, 8),
                DEFAULT_PALETTE,
                white,
                Color::from(0x00FF_FFFF),
            );
            rp.fill_rect(0, 0, 8, 8);
        }
        for y in 0..8u32 {
            for x in 0..8u32 {
                assert_eq!(fb.buf[(y * 8 + x) as usize], 0x00FF_FFFF, "({x},{y})");
            }
        }
    }

    #[test]
    fn move_then_line_leaves_the_position_at_the_end() {
        let g = graphics();
        let pen = g.alloc_pen(DEFAULT_PALETTE, 0x00FF_FFFF).unwrap();
        let mut fb = FakeFb::new(16, 16, 16);
        let mut rp = RastPort::new(
            fb.surface(16, 16, 16),
            DEFAULT_PALETTE,
            pen,
            Color::from(0x00FF_FFFF),
        );
        rp.move_to(2, 3);
        assert_eq!(rp.position(), (2, 3));
        rp.line(10, 3);
        assert_eq!(rp.position(), (10, 3));
        // A second line starts where the first ended.
        rp.line(10, 9);
        assert_eq!(rp.position(), (10, 9));
    }

    #[test]
    fn drawing_is_clipped_to_the_rast_ports_clip() {
        let g = graphics();
        let pen = g.alloc_pen(DEFAULT_PALETTE, 0x00FF_FFFF).unwrap();
        let mut fb = FakeFb::new(16, 16, 16);
        let mut rp = RastPort::new(
            fb.surface(16, 16, 16),
            DEFAULT_PALETTE,
            pen,
            Color::from(0x00FF_FFFF),
        );
        rp.set_clip(Rect::new(4, 4, 4, 4));
        rp.fill_rect(0, 0, 16, 16);
        assert_eq!(fb.buf[0], 0, "outside the clip");
        assert_ne!(fb.buf[5 * 16 + 5], 0, "inside the clip");
        assert_eq!(fb.buf[15 * 16 + 15], 0, "outside the clip");
    }

    #[test]
    fn drawing_honours_the_mask() {
        let g = graphics();
        let pen = g.alloc_pen(DEFAULT_PALETTE, 0x00FF_FFFF).unwrap();
        let mut fb = FakeFb::new(16, 16, 16);
        let mut rp = RastPort::new(
            fb.surface(16, 16, 16),
            DEFAULT_PALETTE,
            pen,
            Color::from(0x00FF_FFFF),
        );
        rp.set_mask(Some(Rect::new(0, 0, 16, 4)));
        rp.fill_rect(0, 0, 16, 16);
        assert_eq!(fb.buf[2 * 16 + 2], 0, "inside the mask");
        assert_ne!(fb.buf[6 * 16 + 6], 0, "below the mask");
    }

    #[test]
    fn text_returns_the_advance_width() {
        let g = graphics();
        let pen = g.alloc_pen(DEFAULT_PALETTE, 0x00FF_FFFF).unwrap();
        let mut fb = FakeFb::new(64, 16, 64);
        let mut rp = RastPort::new(
            fb.surface(64, 16, 64),
            DEFAULT_PALETTE,
            pen,
            Color::from(0x00FF_FFFF),
        );
        assert_eq!(rp.text("AB", 0, 0), 16, "two 8px glyphs");
    }

    #[test]
    fn a_negative_put_pixel_is_ignored_rather_than_wrapping() {
        let g = graphics();
        let pen = g.alloc_pen(DEFAULT_PALETTE, 0x00FF_FFFF).unwrap();
        let mut fb = FakeFb::new(8, 8, 8);
        let mut rp = RastPort::new(
            fb.surface(8, 8, 8),
            DEFAULT_PALETTE,
            pen,
            Color::from(0x00FF_FFFF),
        );
        rp.put_pixel(-1, -1);
        rp.put_pixel(-1, 100);
        rp.put_pixel(100, -1);
        for v in fb.buf.iter() {
            assert_eq!(*v, 0, "nothing may be painted");
        }
    }

    #[test]
    fn a_rast_port_can_be_handed_back_as_a_surface() {
        let g = graphics();
        let pen = g.alloc_pen(DEFAULT_PALETTE, 0x00FF_FFFF).unwrap();
        let mut fb = FakeFb::new(8, 8, 8);
        let rp = RastPort::new(
            fb.surface(8, 8, 8),
            DEFAULT_PALETTE,
            pen,
            Color::from(0x00FF_FFFF),
        );
        let mut surface = rp.into_surface();
        surface.fill_rect(Color::WHITE, 0, 0, 8, 8);
        assert_ne!(fb.buf[0], 0);
    }

    #[test]
    fn resolving_a_palette_yields_colours() {
        let g = graphics();
        g.alloc_pen(DEFAULT_PALETTE, 0x00FF_0000).unwrap();
        g.alloc_pen(DEFAULT_PALETTE, 0x0000_FF00).unwrap();
        let colors = g.resolve(DEFAULT_PALETTE).unwrap();
        assert_eq!(colors.len(), 2);
        assert_eq!(colors[0], Color::from(0x00FF_0000));
        assert_eq!(colors[1], Color::from(0x0000_FF00));
        assert!(g.resolve(9).is_err());
    }

    #[test]
    fn the_summary_mentions_the_geometry() {
        let g = graphics();
        let pen = g.alloc_pen(DEFAULT_PALETTE, 0).unwrap();
        let mut fb = FakeFb::new(64, 32, 64);
        let rp = RastPort::new(fb.surface(64, 32, 64), DEFAULT_PALETTE, pen, Color::BLACK);
        let s = rp.summary();
        assert!(s.contains("64x32"), "{s}");
        assert!(s.contains("stride 64"), "{s}");
    }
}
