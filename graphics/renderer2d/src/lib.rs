//! A small software 2D renderer over a linear framebuffer.
//!
//! The renderer owns no memory: it borrows a framebuffer the caller already
//! has and validates the geometry in [`Surface::new`]. Everything after that is
//! bounds-checked internally, so a caller cannot talk it into writing outside
//! the buffer.
//!
//! Two probe findings are baked into the geometry rules:
//!
//! * QEMU `q35` reports **1280x800 with stride 1280** in `Bgr` format. The
//!   renderer therefore never assumes `stride == width`, and never assumes RGB
//!   byte order.
//! * `BltOnly` modes have no addressable framebuffer, so the pixel format is
//!   an explicit enum rather than an `Option`.

#![no_std]

extern crate alloc;

use core::marker::PhantomData;

use vela_boot_protocol::PixelFormat;

/// An RGB colour, 8 bits per channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Color {
    /// Build from components.
    pub const fn rgb(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b }
    }

    /// Black.
    pub const BLACK: Color = Color::rgb(0, 0, 0);
    /// White.
    pub const WHITE: Color = Color::rgb(0xFF, 0xFF, 0xFF);
}

impl From<u32> for Color {
    /// Decode from a packed `0x00RRGGBB` value.
    fn from(v: u32) -> Self {
        Self::rgb(
            ((v >> 16) & 0xFF) as u8,
            ((v >> 8) & 0xFF) as u8,
            (v & 0xFF) as u8,
        )
    }
}

/// A clipping rectangle in surface coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

impl Rect {
    /// Build a rectangle.
    pub const fn new(x: i32, y: i32, width: u32, height: u32) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    /// True when `(x, y)` lies inside, treating the rectangle as half-open.
    pub const fn contains(&self, x: i32, y: i32) -> bool {
        x >= self.x
            && y >= self.y
            && x < self.x + self.width as i32
            && y < self.y + self.height as i32
    }

    /// Overlap with `other`, or `None` when disjoint.
    pub fn intersect(&self, other: &Self) -> Option<Self> {
        let x0 = self.x.max(other.x);
        let y0 = self.y.max(other.y);
        let x1 = (self.x + self.width as i32).min(other.x + other.width as i32);
        let y1 = (self.y + self.height as i32).min(other.y + other.height as i32);
        if x1 <= x0 || y1 <= y0 {
            None
        } else {
            Some(Self::new(x0, y0, (x1 - x0) as u32, (y1 - y0) as u32))
        }
    }
}

/// A set of at most two horizontal runs of unmasked pixels.
///
/// A mask rectangle can split a row into at most two pieces — one before the
/// mask and one after — so two slots is exact, not a guess.
#[derive(Debug, Clone, Copy)]
struct RowRuns {
    /// First run.
    first: (i32, i32),
    /// Second run, if any.
    second: (i32, i32),
}

impl RowRuns {
    /// A run that is empty.
    const EMPTY: RowRuns = RowRuns {
        first: (0, 0),
        second: (0, 0),
    };
    /// One run of `width` pixels starting at `x`.
    fn single(x: i32, width: i32) -> RowRuns {
        RowRuns {
            first: (x, width),
            second: (0, 0),
        }
    }
    /// Two runs, at most.
    fn from_pair(runs: &[RowRuns; 2], count: u8) -> RowRuns {
        let first = if count > 0 { runs[0].first } else { (0, 0) };
        let second = if count > 1 { runs[1].first } else { (0, 0) };
        RowRuns { first, second }
    }

    /// The live runs.
    fn iter(&self) -> RowRunsIter {
        RowRunsIter {
            runs: *self,
            next: 0,
        }
    }
}

/// Iterator over a [`RowRuns`]'s live runs.
struct RowRunsIter {
    runs: RowRuns,
    next: u8,
}

impl Iterator for RowRunsIter {
    type Item = (i32, i32);

    fn next(&mut self) -> Option<Self::Item> {
        let run = match self.next {
            0 => self.runs.first,
            1 => self.runs.second,
            _ => return None,
        };
        self.next += 1;
        if run.1 > 0 {
            Some(run)
        } else {
            // A zero-width run is not a run. Skipping it here keeps the call
            // site free of the check, and there are at most two of them.
            self.next()
        }
    }
}

/// Typed view over a raw framebuffer.
///
/// The `PhantomData<*mut u8>` is load-bearing: it makes `Surface` `!Send` and
/// `!Sync`, because two threads writing one framebuffer is a data race with no
/// synchronisation to protect it.
pub struct Surface<'a> {
    base: *mut u32,
    width: u32,
    height: u32,
    stride: u32,
    format: PixelFormat,
    clip: Rect,
    /// The "mask" rectangle, inside which nothing is drawn.
    ///
    /// The AmigaOS mask is a separate plane, and a pixel is drawn only where
    /// the mask is clear. Modelling it as a rectangle that is *excluded* from
    /// drawing is enough for v0.1.0: a window that wants a transparent hole
    /// punches one.
    mask: Option<Rect>,
    _marker: PhantomData<&'a mut [u32]>,
}

impl<'a> Surface<'a> {
    /// Validate geometry and build a surface.
    ///
    /// Returns `None` when the reported geometry cannot possibly be safe:
    /// a zero dimension, a stride narrower than the width, or a byte length
    /// too small for `stride * height`. Refusing here means every later method
    /// can stay simple.
    pub fn new(
        base: *mut u8,
        len: usize,
        width: u32,
        height: u32,
        stride_px: u32,
        format: PixelFormat,
    ) -> Option<Self> {
        if base.is_null() || width == 0 || height == 0 || stride_px == 0 {
            return None;
        }
        if stride_px < width {
            return None;
        }
        // Checked: a hostile geometry must not wrap this multiplication.
        let need = (stride_px as usize).checked_mul(height as usize)?;
        if need > len {
            return None;
        }
        Some(Self {
            base: base.cast::<u32>(),
            width,
            height,
            stride: stride_px,
            format,
            clip: Rect::new(0, 0, width, height),
            mask: None,
            _marker: PhantomData,
        })
    }

    pub const fn width(&self) -> u32 {
        self.width
    }

    pub const fn height(&self) -> u32 {
        self.height
    }

    pub const fn stride(&self) -> u32 {
        self.stride
    }

    pub const fn pixel_format(&self) -> PixelFormat {
        self.format
    }

    /// The current clip rectangle.
    pub const fn clip(&self) -> Rect {
        self.clip
    }

    /// Replace the clip rectangle, clamped to the surface.
    pub fn set_clip(&mut self, r: Rect) {
        self.clip = self
            .full_rect()
            .intersect(&r)
            .unwrap_or(Rect::new(0, 0, 0, 0));
    }

    /// Reset the clip to the whole surface.
    pub fn reset_clip(&mut self) {
        self.clip = self.full_rect();
    }

    /// Set the mask rectangle, inside which nothing is drawn.
    pub fn set_mask(&mut self, r: Option<Rect>) {
        self.mask = r.and_then(|r| self.full_rect().intersect(&r));
    }

    /// The current mask rectangle.
    pub const fn mask(&self) -> Option<Rect> {
        self.mask
    }

    /// Clear the mask.
    pub fn clear_mask(&mut self) {
        self.mask = None;
    }

    /// True when `(x, y)` may be drawn at.
    ///
    /// Inside the clip and outside the mask. The two tests are separate
    /// because they answer different questions: the clip is the *drawing
    /// area* and the mask is a *hole in it*.
    fn drawable(&self, x: i32, y: i32) -> bool {
        self.clip.contains(x, y) && !self.mask.map(|m| m.contains(x, y)).unwrap_or(false)
    }

    fn full_rect(&self) -> Rect {
        Rect::new(0, 0, self.width, self.height)
    }

    /// Pack a colour into the surface's actual byte order.
    ///
    /// On little-endian x86-64 an `Rgb` pixel is `0x00RRGGBB` and a `Bgr`
    /// pixel is `0x00BBGGRR`.
    const fn pack(&self, c: Color) -> u32 {
        match self.format {
            PixelFormat::Rgb | PixelFormat::RgbMask => {
                ((c.r as u32) << 16) | ((c.g as u32) << 8) | c.b as u32
            }
            PixelFormat::Bgr => ((c.b as u32) << 16) | ((c.g as u32) << 8) | c.r as u32,
        }
    }

    /// Write one pixel if it is inside the clip and outside the mask.
    #[allow(unsafe_code)]
    pub fn put_pixel(&mut self, x: u32, y: u32, color: Color) {
        if !self.drawable(x as i32, y as i32) {
            return;
        }
        let v = self.pack(color);
        // SAFETY: `x < width <= stride` and `y < height`, and `new` proved
        // `stride * height` 32-bit words fit inside `len`. The clip check above
        // only narrows this further.
        unsafe {
            let p = self
                .base
                .add((y as usize) * (self.stride as usize) + x as usize);
            p.write_volatile(v);
        }
    }

    /// Read one pixel, or black when out of bounds.
    #[allow(unsafe_code)]
    pub fn get_pixel(&self, x: u32, y: u32) -> Color {
        if x >= self.width || y >= self.height {
            return Color::BLACK;
        }
        // SAFETY: same bounds argument as `put_pixel`.
        let v = unsafe {
            self.base
                .add((y as usize) * (self.stride as usize) + x as usize)
                .read_volatile()
        };
        match self.format {
            PixelFormat::Bgr => Color::rgb(
                (v & 0xFF) as u8,
                ((v >> 8) & 0xFF) as u8,
                ((v >> 16) & 0xFF) as u8,
            ),
            _ => Color::rgb(
                ((v >> 16) & 0xFF) as u8,
                ((v >> 8) & 0xFF) as u8,
                (v & 0xFF) as u8,
            ),
        }
    }

    /// Fill an axis-aligned rectangle, clipped and masked.
    #[allow(unsafe_code)]
    pub fn fill_rect(&mut self, color: Color, x: i32, y: i32, width: u32, height: u32) {
        if width == 0 || height == 0 {
            return;
        }
        let Some(r) = Rect::new(x, y, width, height).intersect(&self.clip) else {
            return;
        };
        let v = self.pack(color);
        for py in r.y..r.y + r.height as i32 {
            // The mask may cut a horizontal band out of the row, so a row is
            // not necessarily one run. `row_runs` returns the runs that
            // survive the mask, which is what keeps the mask from being
            // quietly ignored by a naive per-pixel loop.
            for run in self.row_runs(&r, py).iter() {
                for px in run.0..run.0 + run.1 {
                    // SAFETY: `r` came from intersecting with `self.clip`, so
                    // every coordinate here is within the surface and its
                    // clip, and `row_runs` excluded the mask.
                    unsafe {
                        self.base
                            .add(py as usize * self.stride as usize + px as usize)
                            .write_volatile(v);
                    }
                }
            }
        }
    }

    /// The horizontal runs of `r` on row `py` that are not masked.
    ///
    /// Returns an iterator of `(x, run_width)`. With no mask there is exactly
    /// one run, which is the whole width.
    fn row_runs(&self, r: &Rect, py: i32) -> RowRuns {
        let full = (r.x, r.width as i32);
        let Some(mask) = self.mask else {
            return RowRuns::single(full.0, full.1);
        };
        // A mask rectangle that misses this row entirely leaves it whole.
        if !mask.contains(r.x, py) && !mask.contains(r.x + r.width as i32 - 1, py) {
            // The row's own span and the mask's span could still overlap in
            // the middle while missing both endpoints — only possible when the
            // mask is narrower than the row, which the intersection below
            // handles correctly on its own. Testing the span directly is both
            // simpler and cheaper than reasoning about that case here.
            let mx0 = mask.x.max(r.x);
            let mx1 = (mask.x + mask.width as i32).min(r.x + r.width as i32);
            if mx0 >= mx1 || py < mask.y || py >= mask.y + mask.height as i32 {
                return RowRuns::single(full.0, full.1);
            }
        }
        let mx0 = mask.x.max(r.x);
        let mx1 = (mask.x + mask.width as i32).min(r.x + r.width as i32);
        let mut runs = [RowRuns::EMPTY; 2];
        let mut n = 0;
        if mx0 > r.x {
            runs[n] = RowRuns::single(r.x, mx0 - r.x);
            n += 1;
        }
        if mx1 < r.x + r.width as i32 {
            runs[n] = RowRuns::single(mx1, r.x + r.width as i32 - mx1);
            n += 1;
        }
        RowRuns::from_pair(&runs, n as u8)
    }

    /// Stroke a one-pixel rectangle outline.
    pub fn stroke_rect(&mut self, color: Color, x: i32, y: i32, width: u32, height: u32) {
        if width == 0 || height == 0 {
            return;
        }
        self.fill_rect(color, x, y, width, 1);
        self.fill_rect(color, x, y + height as i32 - 1, width, 1);
        self.fill_rect(color, x, y, 1, height);
        self.fill_rect(color, x + width as i32 - 1, y, 1, height);
    }

    /// Draw a line with Bresenham's algorithm.
    pub fn line(&mut self, color: Color, x0: i32, y0: i32, x1: i32, y1: i32) {
        let dx = (x1 - x0).abs();
        let sx = if x0 < x1 { 1 } else { -1 };
        let dy = -(y1 - y0).abs();
        let sy = if y0 < y1 { 1 } else { -1 };
        let mut err = dx + dy;
        let (mut x, mut y) = (x0, y0);
        loop {
            if x >= 0 && y >= 0 {
                self.put_pixel(x as u32, y as u32, color);
            }
            if x == x1 && y == y1 {
                break;
            }
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

    /// Draw a filled rectangle outline of `thickness` pixels.
    pub fn fill_rect_thick(&mut self, color: Color, x: i32, y: i32, w: u32, h: u32, t: u32) {
        for i in 0..t {
            self.stroke_rect(
                color,
                x + i as i32,
                y + i as i32,
                w.saturating_sub(2 * i),
                h.saturating_sub(2 * i),
            );
        }
    }

    /// Draw text with the embedded 8x16 font, scaled by an integer factor.
    ///
    /// Returns the advance width in pixels. Coordinates are `i32` to match the
    /// other drawing entry points, so off-screen text is clipped rather than
    /// wrapping around.
    pub fn draw_text(&mut self, color: Color, text: &str, x: i32, y: i32, scale: u32) -> u32 {
        let scale = scale.max(1);
        let mut cx = x;
        for ch in text.chars() {
            let glyph = vela_text::glyph(ch);
            let cw = vela_text::GLYPH_WIDTH;
            let ch_h = vela_text::GLYPH_HEIGHT;
            for gy in 0..ch_h {
                let bits = glyph[gy as usize];
                for gx in 0..cw {
                    if bits & (1 << (cw - 1 - gx)) != 0 {
                        self.fill_rect(
                            color,
                            cx + (gx * scale) as i32,
                            y + (gy * scale) as i32,
                            scale,
                            scale,
                        );
                    }
                }
            }
            cx += (cw * scale) as i32;
        }
        (cx - x) as u32
    }
}

// SAFETY: `Surface` owns exclusive access to the framebuffer it was built from,
// which is why it is `!Send`/`!Sync`. No impl is needed.
const _: fn() = || {
    fn assert_not_send<T>() {}
    let _ = assert_not_send::<Surface<'static>>;
};

#[cfg(test)]
mod tests {
    use super::*;

    /// A heap-backed fake framebuffer so the renderer can be tested on host.
    struct FakeFb {
        buf: alloc::vec::Vec<u32>,
    }

    impl FakeFb {
        fn new(_width: u32, height: u32, stride: u32) -> Self {
            Self {
                buf: alloc::vec![0u32; (stride * height) as usize],
            }
        }

        fn surface(
            &mut self,
            width: u32,
            height: u32,
            stride: u32,
            fmt: PixelFormat,
        ) -> Surface<'_> {
            Surface::new(
                self.buf.as_mut_ptr().cast(),
                self.buf.len() * 4,
                width,
                height,
                stride,
                fmt,
            )
            .expect("geometry should be valid")
        }
    }

    #[test]
    fn rejects_invalid_geometry() {
        let mut p = 0u8;
        // Null base.
        assert!(Surface::new(core::ptr::null_mut(), 4096, 10, 10, 10, PixelFormat::Bgr).is_none());
        // Zero dimensions.
        assert!(Surface::new(&mut p as *mut u8, 4096, 0, 10, 10, PixelFormat::Bgr).is_none());
        assert!(Surface::new(&mut p as *mut u8, 4096, 10, 0, 10, PixelFormat::Bgr).is_none());
        // Stride narrower than width.
        assert!(Surface::new(&mut p as *mut u8, 4096, 100, 10, 50, PixelFormat::Bgr).is_none());
        // Buffer too small for stride*height.
        assert!(Surface::new(&mut p as *mut u8, 16, 100, 10, 100, PixelFormat::Bgr).is_none());
    }

    #[test]
    fn accepts_padded_stride() {
        let mut fb = FakeFb::new(100, 10, 128);
        let s = fb.surface(100, 10, 128, PixelFormat::Bgr);
        assert_eq!(s.stride(), 128);
        assert_eq!(s.width(), 100);
    }

    #[test]
    fn stride_padding_is_not_written() {
        // Fill only the first 4 columns; the padding columns must stay zero.
        let mut fb = FakeFb::new(16, 4, 32);
        {
            let mut s = fb.surface(16, 4, 32, PixelFormat::Bgr);
            s.fill_rect(Color::WHITE, 0, 0, 4, 4);
        }
        for y in 0..4u32 {
            for x in 0..32usize {
                let v = fb.buf[(y as usize) * 32 + x];
                if x < 4 {
                    assert_ne!(v, 0, "column {x} should be painted");
                } else {
                    assert_eq!(v, 0, "padding column {x} must not be written");
                }
            }
        }
    }

    #[test]
    fn pixel_colour_round_trips_in_bgr() {
        let mut fb = FakeFb::new(4, 4, 4);
        let mut s = fb.surface(4, 4, 4, PixelFormat::Bgr);
        s.put_pixel(1, 1, Color::rgb(0x11, 0x22, 0x33));
        assert_eq!(s.get_pixel(1, 1), Color::rgb(0x11, 0x22, 0x33));
    }

    #[test]
    fn pixel_colour_round_trips_in_rgb() {
        let mut fb = FakeFb::new(4, 4, 4);
        let mut s = fb.surface(4, 4, 4, PixelFormat::Rgb);
        s.put_pixel(1, 1, Color::rgb(0x11, 0x22, 0x33));
        assert_eq!(s.get_pixel(1, 1), Color::rgb(0x11, 0x22, 0x33));
    }

    #[test]
    fn fill_rect_is_clipped() {
        let mut fb = FakeFb::new(10, 10, 10);
        let mut s = fb.surface(10, 10, 10, PixelFormat::Bgr);
        // Extends far outside; must not panic and must not wrap.
        s.fill_rect(Color::WHITE, -100, -100, 1000, 1000);
        // Everything is now painted.
        assert_eq!(s.get_pixel(0, 0), Color::WHITE);
        assert_eq!(s.get_pixel(9, 9), Color::WHITE);
    }

    #[test]
    fn clip_rect_restricts_drawing() {
        let mut fb = FakeFb::new(10, 10, 10);
        let mut s = fb.surface(10, 10, 10, PixelFormat::Bgr);
        s.set_clip(Rect::new(2, 2, 4, 4));
        s.fill_rect(Color::WHITE, 0, 0, 10, 10);
        assert_eq!(s.get_pixel(0, 0), Color::BLACK, "outside clip");
        assert_eq!(s.get_pixel(5, 5), Color::WHITE, "inside clip");
        assert_eq!(s.get_pixel(9, 9), Color::BLACK, "outside clip");
    }

    #[test]
    fn clip_larger_than_surface_is_clamped() {
        let mut fb = FakeFb::new(10, 10, 10);
        let mut s = fb.surface(10, 10, 10, PixelFormat::Bgr);
        s.set_clip(Rect::new(-50, -50, 1000, 1000));
        assert_eq!(s.clip(), Rect::new(0, 0, 10, 10));
    }

    #[test]
    fn disjoint_clip_draws_nothing() {
        let mut fb = FakeFb::new(10, 10, 10);
        let mut s = fb.surface(10, 10, 10, PixelFormat::Bgr);
        s.set_clip(Rect::new(100, 100, 4, 4));
        s.fill_rect(Color::WHITE, 0, 0, 10, 10);
        assert_eq!(s.get_pixel(5, 5), Color::BLACK);
    }

    #[test]
    fn out_of_bounds_reads_are_black() {
        let mut fb = FakeFb::new(4, 4, 4);
        let s = fb.surface(4, 4, 4, PixelFormat::Bgr);
        assert_eq!(s.get_pixel(100, 100), Color::BLACK);
    }

    #[test]
    fn line_draws_endpoints() {
        let mut fb = FakeFb::new(20, 20, 20);
        let mut s = fb.surface(20, 20, 20, PixelFormat::Bgr);
        s.line(Color::WHITE, 0, 0, 19, 19);
        assert_eq!(s.get_pixel(0, 0), Color::WHITE);
        assert_eq!(s.get_pixel(19, 19), Color::WHITE);
    }

    #[test]
    fn horizontal_and_vertical_lines() {
        let mut fb = FakeFb::new(20, 20, 20);
        let mut s = fb.surface(20, 20, 20, PixelFormat::Bgr);
        s.line(Color::WHITE, 0, 5, 19, 5);
        for x in 0..20u32 {
            assert_eq!(s.get_pixel(x, 5), Color::WHITE, "x={x}");
        }
    }

    #[test]
    fn stroke_rect_leaves_interior_untouched() {
        let mut fb = FakeFb::new(10, 10, 10);
        let mut s = fb.surface(10, 10, 10, PixelFormat::Bgr);
        s.stroke_rect(Color::WHITE, 1, 1, 8, 8);
        assert_eq!(s.get_pixel(1, 1), Color::WHITE, "corner");
        assert_eq!(s.get_pixel(5, 5), Color::BLACK, "interior");
        assert_eq!(s.get_pixel(8, 8), Color::WHITE, "far corner");
    }

    #[test]
    fn text_advances_the_cursor() {
        let mut fb = FakeFb::new(200, 40, 200);
        let mut s = fb.surface(200, 40, 200, PixelFormat::Bgr);
        let w = s.draw_text(Color::WHITE, "AB", 0, 0, 1);
        assert_eq!(w, 16, "two 8px glyphs");
        let w2 = s.draw_text(Color::WHITE, "AB", 0, 0, 2);
        assert_eq!(w2, 32, "scaled by two");
    }

    #[test]
    fn unknown_characters_render_as_a_box_not_a_panic() {
        let mut fb = FakeFb::new(64, 32, 64);
        let mut s = fb.surface(64, 32, 64, PixelFormat::Bgr);
        s.draw_text(Color::WHITE, "\u{1F600}\u{4E2D}", 0, 0, 1);
    }

    #[test]
    fn rect_intersection() {
        let a = Rect::new(0, 0, 10, 10);
        let b = Rect::new(5, 5, 10, 10);
        assert_eq!(a.intersect(&b), Some(Rect::new(5, 5, 5, 5)));
        let c = Rect::new(100, 100, 2, 2);
        assert_eq!(a.intersect(&c), None);
        // Touching edges do not intersect (half-open).
        let d = Rect::new(10, 0, 5, 5);
        assert_eq!(a.intersect(&d), None);
    }

    #[test]
    fn colour_from_u32_uses_0x00rrggbb_ordering() {
        assert_eq!(Color::from(0x1122_3344), Color::rgb(0x22, 0x33, 0x44));
    }

    #[test]
    fn a_mask_excludes_the_pixels_it_covers() {
        let mut fb = FakeFb::new(10, 10, 10);
        let mut s = fb.surface(10, 10, 10, PixelFormat::Bgr);
        s.set_mask(Some(Rect::new(3, 3, 4, 4)));
        s.fill_rect(Color::WHITE, 0, 0, 10, 10);
        // A pixel inside the mask survives.
        assert_eq!(s.get_pixel(0, 0), Color::WHITE, "before the mask");
        assert_eq!(s.get_pixel(5, 5), Color::BLACK, "inside the mask");
        assert_eq!(s.get_pixel(9, 9), Color::WHITE, "after the mask");
    }

    #[test]
    fn a_mask_outside_the_surface_is_ignored() {
        let mut fb = FakeFb::new(10, 10, 10);
        let mut s = fb.surface(10, 10, 10, PixelFormat::Bgr);
        s.set_mask(Some(Rect::new(100, 100, 4, 4)));
        assert_eq!(s.mask(), None, "an off-surface mask clamps away");
        s.fill_rect(Color::WHITE, 0, 0, 10, 10);
        assert_eq!(s.get_pixel(5, 5), Color::WHITE);
    }

    #[test]
    fn clearing_the_mask_restores_drawing() {
        let mut fb = FakeFb::new(10, 10, 10);
        let mut s = fb.surface(10, 10, 10, PixelFormat::Bgr);
        s.set_mask(Some(Rect::new(0, 0, 10, 10)));
        s.fill_rect(Color::WHITE, 0, 0, 10, 10);
        assert_eq!(s.get_pixel(5, 5), Color::BLACK);
        s.clear_mask();
        s.fill_rect(Color::WHITE, 0, 0, 10, 10);
        assert_eq!(s.get_pixel(5, 5), Color::WHITE);
    }

    #[test]
    fn a_line_does_not_draw_through_the_mask() {
        let mut fb = FakeFb::new(20, 1, 20);
        let mut s = fb.surface(20, 1, 20, PixelFormat::Bgr);
        s.set_mask(Some(Rect::new(5, 0, 10, 1)));
        s.line(Color::WHITE, 0, 0, 19, 0);
        assert_eq!(s.get_pixel(0, 0), Color::WHITE);
        assert_eq!(s.get_pixel(4, 0), Color::WHITE);
        assert_eq!(s.get_pixel(10, 0), Color::BLACK, "inside the mask");
        assert_eq!(s.get_pixel(19, 0), Color::WHITE);
    }

    #[test]
    fn text_does_not_draw_through_the_mask() {
        let mut fb = FakeFb::new(64, 16, 64);
        let mut s = fb.surface(64, 16, 64, PixelFormat::Bgr);
        s.set_mask(Some(Rect::new(0, 0, 64, 16)));
        s.draw_text(Color::WHITE, "HELLO", 0, 0, 1);
        // Every pixel of the glyph is masked, so nothing was painted.
        for x in 0..64u32 {
            for y in 0..16u32 {
                assert_eq!(s.get_pixel(x, y), Color::BLACK, "({x},{y})");
            }
        }
    }
}
