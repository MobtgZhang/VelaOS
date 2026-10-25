//! VelaOS's own 8x16 bitmap font.
//!
//! # Provenance
//!
//! These glyphs are **original to VelaOS**. They are not a copy, trace, or
//! derivative of the Amiga Topaz 8, Amiga Roman, or any other historic font —
//! those bitmap data blobs are the property of their respective owners and are
//! deliberately not reproduced here. Every glyph below was constructed from a
//! stroke description in [`strokes`] and rasterised by hand.
//!
//! # Format
//!
//! Each glyph is [`GLYPH_HEIGHT`] rows of [`GLYPH_WIDTH`] pixels, packed one
//! row per byte with the **leftmost pixel in the most significant bit**. That
//! makes the tables readable and diffable in review, which matters for an
//! embedded asset nobody can regenerate from a TTF at boot.

// The font is linked into the kernel image, which runs with no standard
// library, so this crate must be `no_std` like everything shipped alongside it.
#![no_std]

extern crate alloc;

/// Glyph width in pixels.
pub const GLYPH_WIDTH: u32 = 8;
/// Glyph height in pixels.
pub const GLYPH_HEIGHT: u32 = 16;

/// ASCII printable range covered by the font.
pub const FIRST_CHAR: char = ' ';
pub const LAST_CHAR: char = '~';
/// Number of glyphs.
pub const GLYPH_COUNT: usize = (LAST_CHAR as usize) - (FIRST_CHAR as usize) + 1;

/// Row data for one glyph.
pub type Glyph = [u8; GLYPH_HEIGHT as usize];

/// A glyph plus its advance width, for future proportional support.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GlyphInfo {
    pub bitmap: Glyph,
    pub advance: u32,
}

/// Return the glyph for `ch`.
///
/// Unrepresentable characters map to the `?` glyph rather than panicking: a
/// boot screen must never crash on unexpected input.
pub fn glyph(ch: char) -> Glyph {
    let cp = ch as u32;
    if cp < FIRST_CHAR as u32 || cp > LAST_CHAR as u32 {
        return question_mark();
    }
    font_table()[(cp - FIRST_CHAR as u32) as usize]
}

/// The glyph substituted for characters outside the printable ASCII range.
///
/// Identical to the table's `'?'` entry by construction, so a screen full of
/// unrepresentable input still renders as readable "?" characters rather than
/// as something arbitrary.
fn question_mark() -> Glyph {
    let mut b = [0u8; 16];
    let f: fn(&mut [u8; 16]) = |b: &mut [u8; 16]| {
        l(b, 2, 4, 5, 4);
        l(b, 5, 4, 5, 8);
        l(b, 5, 8, 3, 10);
        l(b, 3, 12, 3, 12);
    };
    f(&mut b);
    b
}

/// Return the glyph plus advance width.
pub fn glyph_info(ch: char) -> GlyphInfo {
    GlyphInfo {
        bitmap: glyph(ch),
        advance: GLYPH_WIDTH,
    }
}

/// Pixel width of `text` when drawn at `scale`.
pub fn text_width(text: &str, scale: u32) -> u32 {
    text.chars().count() as u32 * GLYPH_WIDTH * scale.max(1)
}

/// Rasterise a set of strokes into a glyph.
///
/// Each stroke is `(x0, y0, x1, y1)` in pixel coordinates, drawn as a
/// thickness-1 Bresenham line. Using a description rather than hand-placed
/// bytes keeps the shapes consistent and makes the table auditable.
mod raster {
    /// Cell layout for the 8x16 grid: x in 0..8, y in 0..16.
    ///
    /// Uppercase letters occupy rows 3..12; lowercase rows 6..12; descenders
    /// extend to row 14.
    pub const fn line(bitmap: &mut [u8; 16], x0: u8, y0: u8, x1: u8, y1: u8) {
        let mut x = x0 as i32;
        let mut y = y0 as i32;
        let (tx, ty) = (x1 as i32, y1 as i32);
        let dx = (tx - x).abs();
        let sx = if x < tx { 1 } else { -1 };
        let dy = -(ty - y).abs();
        let sy = if y < ty { 1 } else { -1 };
        let mut err = dx + dy;
        loop {
            if x >= 0 && x < 8 && y >= 0 && y < 16 {
                bitmap[y as usize] |= 1 << (7 - x);
            }
            if x == tx && y == ty {
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
}

macro_rules! build {
    ($($ch:literal => $body:expr),* $(,)?) => {{
        let mut table = [[0u8; 16]; GLYPH_COUNT];
        let mut idx = 0usize;
        $(
            {
                let mut b = [0u8; 16];
                let f: fn(&mut [u8; 16]) = $body;
                f(&mut b);
                table[idx] = b;
            }
            idx += 1;
        )*
        table
    }};
}

use raster::line as l;

/// The font table, one [`Glyph`] per printable ASCII code point.
///
/// Built on first use rather than as a plain `static`, because the glyph shapes
/// come from calling the rasteriser through a function pointer, which `const`
/// evaluation cannot do. `spin::Once` keeps this available in `no_std`.
fn font_table() -> &'static [Glyph; GLYPH_COUNT] {
    use spin::Once;
    static TABLE: Once<[Glyph; GLYPH_COUNT]> = Once::new();
    TABLE.call_once(|| build! {
    ' ' => |b: &mut [u8; 16]| { let _ = b; },
    '!' => |b: &mut [u8; 16]| { l(b, 3, 3, 3, 10); l(b, 3, 12, 3, 12); },
    '"' => |b: &mut [u8; 16]| { l(b, 2, 3, 2, 6); l(b, 5, 3, 5, 6); },
    '#' => |b: &mut [u8; 16]| { l(b, 2, 4, 2, 11); l(b, 5, 4, 5, 11); l(b, 1, 6, 6, 6); l(b, 1, 9, 6, 9); },
    '$' => |b: &mut [u8; 16]| { l(b, 4, 2, 4, 13); l(b, 1, 5, 6, 5); l(b, 1, 9, 6, 9); l(b, 2, 4, 5, 4); l(b, 2, 10, 5, 10); },
    '%' => |b: &mut [u8; 16]| { l(b, 1, 3, 6, 12); l(b, 1, 3, 3, 5); l(b, 1, 3, 1, 5); l(b, 3, 3, 3, 5); l(b, 4, 10, 6, 12); l(b, 6, 10, 4, 12); l(b, 4, 10, 4, 12); l(b, 4, 10, 6, 10); },
    '&' => |b: &mut [u8; 16]| { l(b, 6, 12, 2, 5); l(b, 2, 5, 4, 3); l(b, 4, 3, 5, 5); l(b, 5, 5, 2, 10); l(b, 2, 10, 4, 12); l(b, 4, 12, 6, 9); },
    '\'' => |b: &mut [u8; 16]| { l(b, 3, 3, 3, 6); },
    '(' => |b: &mut [u8; 16]| { l(b, 5, 2, 3, 5); l(b, 3, 5, 3, 10); l(b, 3, 10, 5, 13); },
    ')' => |b: &mut [u8; 16]| { l(b, 2, 2, 4, 5); l(b, 4, 5, 4, 10); l(b, 4, 10, 2, 13); },
    '*' => |b: &mut [u8; 16]| { l(b, 4, 5, 4, 10); l(b, 1, 6, 6, 9); l(b, 6, 6, 1, 9); },
    '+' => |b: &mut [u8; 16]| { l(b, 4, 5, 4, 10); l(b, 1, 7, 6, 7); },
    ',' => |b: &mut [u8; 16]| { l(b, 4, 10, 4, 12); l(b, 4, 12, 2, 14); },
    '-' => |b: &mut [u8; 16]| { l(b, 1, 7, 6, 7); },
    '.' => |b: &mut [u8; 16]| { l(b, 3, 11, 3, 12); },
    '/' => |b: &mut [u8; 16]| { l(b, 1, 12, 6, 3); },
    // A narrow oval, deliberately tighter than `8` so the two digits are
// distinguishable at 8x16.
'0' => |b: &mut [u8; 16]| { l(b, 3, 4, 4, 4); l(b, 4, 4, 4, 11); l(b, 4, 11, 3, 11); l(b, 3, 11, 3, 4); l(b, 3, 8, 4, 7); },
    '1' => |b: &mut [u8; 16]| { l(b, 2, 5, 4, 3); l(b, 4, 3, 4, 11); l(b, 2, 11, 6, 11); },
    '2' => |b: &mut [u8; 16]| { l(b, 2, 4, 5, 4); l(b, 5, 4, 5, 8); l(b, 5, 8, 2, 11); l(b, 2, 11, 6, 11); },
    '3' => |b: &mut [u8; 16]| { l(b, 2, 4, 5, 4); l(b, 5, 4, 5, 11); l(b, 5, 11, 2, 11); l(b, 3, 8, 5, 8); },
    '4' => |b: &mut [u8; 16]| { l(b, 5, 3, 1, 9); l(b, 1, 9, 6, 9); l(b, 5, 3, 5, 12); },
    '5' => |b: &mut [u8; 16]| { l(b, 6, 3, 2, 3); l(b, 2, 3, 2, 7); l(b, 2, 7, 5, 7); l(b, 5, 7, 5, 11); l(b, 5, 11, 2, 11); },
    '6' => |b: &mut [u8; 16]| { l(b, 5, 3, 2, 6); l(b, 2, 6, 2, 11); l(b, 2, 11, 5, 11); l(b, 5, 11, 5, 7); l(b, 5, 7, 2, 7); },
    '7' => |b: &mut [u8; 16]| { l(b, 1, 3, 6, 3); l(b, 6, 3, 3, 12); },
    '8' => |b: &mut [u8; 16]| { l(b, 2, 7, 2, 4); l(b, 2, 4, 5, 4); l(b, 5, 4, 5, 7); l(b, 5, 7, 2, 7); l(b, 2, 7, 2, 11); l(b, 2, 11, 5, 11); l(b, 5, 11, 5, 7); l(b, 2, 11, 2, 7); },
    '9' => |b: &mut [u8; 16]| { l(b, 2, 11, 5, 8); l(b, 5, 8, 5, 4); l(b, 5, 4, 2, 4); l(b, 2, 4, 2, 8); l(b, 2, 8, 5, 8); },
    ':' => |b: &mut [u8; 16]| { l(b, 3, 5, 3, 6); l(b, 3, 10, 3, 11); },
    ';' => |b: &mut [u8; 16]| { l(b, 3, 5, 3, 6); l(b, 3, 10, 3, 11); l(b, 3, 11, 2, 13); },
    '<' => |b: &mut [u8; 16]| { l(b, 5, 4, 2, 7); l(b, 2, 7, 5, 10); },
    '=' => |b: &mut [u8; 16]| { l(b, 1, 6, 6, 6); l(b, 1, 9, 6, 9); },
    '>' => |b: &mut [u8; 16]| { l(b, 2, 4, 5, 7); l(b, 5, 7, 2, 10); },
    '?' => |b: &mut [u8; 16]| { l(b, 2, 4, 5, 4); l(b, 5, 4, 5, 8); l(b, 5, 8, 3, 10); l(b, 3, 12, 3, 12); },
    '@' => |b: &mut [u8; 16]| { l(b, 5, 5, 3, 5); l(b, 3, 5, 3, 10); l(b, 3, 10, 5, 10); l(b, 5, 10, 5, 7); l(b, 5, 7, 4, 7); l(b, 1, 6, 1, 9); },
    'A' => |b: &mut [u8; 16]| { l(b, 1, 11, 3, 3); l(b, 3, 3, 5, 11); l(b, 2, 8, 5, 8); },
    'B' => |b: &mut [u8; 16]| { l(b, 2, 3, 2, 11); l(b, 2, 3, 5, 3); l(b, 5, 3, 5, 7); l(b, 5, 7, 2, 7); l(b, 2, 7, 5, 7); l(b, 5, 7, 5, 11); l(b, 5, 11, 2, 11); },
    'C' => |b: &mut [u8; 16]| { l(b, 5, 4, 3, 3); l(b, 3, 3, 2, 5); l(b, 2, 5, 2, 9); l(b, 2, 9, 3, 11); l(b, 3, 11, 5, 10); },
    'D' => |b: &mut [u8; 16]| { l(b, 2, 3, 2, 11); l(b, 2, 3, 4, 3); l(b, 4, 3, 5, 5); l(b, 5, 5, 5, 9); l(b, 5, 9, 4, 11); l(b, 4, 11, 2, 11); },
    'E' => |b: &mut [u8; 16]| { l(b, 2, 3, 2, 11); l(b, 2, 3, 5, 3); l(b, 2, 7, 4, 7); l(b, 2, 11, 5, 11); },
    'F' => |b: &mut [u8; 16]| { l(b, 2, 3, 2, 11); l(b, 2, 3, 5, 3); l(b, 2, 7, 4, 7); },
    'G' => |b: &mut [u8; 16]| { l(b, 5, 4, 3, 3); l(b, 3, 3, 2, 5); l(b, 2, 5, 2, 9); l(b, 2, 9, 3, 11); l(b, 3, 11, 5, 11); l(b, 5, 11, 5, 8); l(b, 5, 8, 3, 8); },
    'H' => |b: &mut [u8; 16]| { l(b, 2, 3, 2, 11); l(b, 5, 3, 5, 11); l(b, 2, 7, 5, 7); },
    'I' => |b: &mut [u8; 16]| { l(b, 2, 3, 5, 3); l(b, 3, 3, 3, 11); l(b, 2, 11, 5, 11); },
    'J' => |b: &mut [u8; 16]| { l(b, 5, 3, 5, 10); l(b, 5, 10, 3, 11); l(b, 3, 11, 2, 10); },
    'K' => |b: &mut [u8; 16]| { l(b, 2, 3, 2, 11); l(b, 5, 3, 2, 8); l(b, 2, 8, 5, 11); },
    'L' => |b: &mut [u8; 16]| { l(b, 2, 3, 2, 11); l(b, 2, 11, 5, 11); },
    'M' => |b: &mut [u8; 16]| { l(b, 1, 11, 1, 3); l(b, 1, 3, 3, 7); l(b, 3, 7, 5, 3); l(b, 5, 3, 5, 11); },
    'N' => |b: &mut [u8; 16]| { l(b, 2, 11, 2, 3); l(b, 2, 3, 5, 11); l(b, 5, 11, 5, 3); },
    'O' => |b: &mut [u8; 16]| { l(b, 3, 3, 2, 5); l(b, 2, 5, 2, 9); l(b, 2, 9, 3, 11); l(b, 3, 11, 5, 11); l(b, 5, 11, 5, 5); l(b, 5, 5, 3, 3); },
    'P' => |b: &mut [u8; 16]| { l(b, 2, 3, 2, 11); l(b, 2, 3, 5, 3); l(b, 5, 3, 5, 7); l(b, 5, 7, 2, 7); },
    'Q' => |b: &mut [u8; 16]| { l(b, 3, 3, 2, 5); l(b, 2, 5, 2, 9); l(b, 2, 9, 3, 11); l(b, 3, 11, 5, 11); l(b, 5, 11, 5, 5); l(b, 5, 5, 3, 3); l(b, 4, 9, 6, 12); },
    'R' => |b: &mut [u8; 16]| { l(b, 2, 3, 2, 11); l(b, 2, 3, 5, 3); l(b, 5, 3, 5, 7); l(b, 5, 7, 2, 7); l(b, 3, 7, 5, 11); },
    'S' => |b: &mut [u8; 16]| { l(b, 5, 4, 3, 3); l(b, 3, 3, 2, 5); l(b, 2, 5, 3, 7); l(b, 3, 7, 5, 8); l(b, 5, 8, 5, 10); l(b, 5, 10, 3, 11); l(b, 3, 11, 1, 10); },
    'T' => |b: &mut [u8; 16]| { l(b, 1, 3, 6, 3); l(b, 3, 3, 3, 11); },
    'U' => |b: &mut [u8; 16]| { l(b, 2, 3, 2, 10); l(b, 2, 10, 3, 11); l(b, 3, 11, 5, 11); l(b, 5, 11, 5, 3); },
    'V' => |b: &mut [u8; 16]| { l(b, 2, 3, 3, 11); l(b, 3, 11, 5, 3); },
    'W' => |b: &mut [u8; 16]| { l(b, 1, 3, 2, 11); l(b, 2, 11, 3, 7); l(b, 3, 7, 5, 11); l(b, 5, 11, 6, 3); },
    'X' => |b: &mut [u8; 16]| { l(b, 2, 3, 5, 11); l(b, 5, 3, 2, 11); },
    'Y' => |b: &mut [u8; 16]| { l(b, 2, 3, 3, 7); l(b, 5, 3, 3, 7); l(b, 3, 7, 3, 11); },
    'Z' => |b: &mut [u8; 16]| { l(b, 2, 3, 5, 3); l(b, 5, 3, 2, 11); l(b, 2, 11, 5, 11); },
    '[' => |b: &mut [u8; 16]| { l(b, 5, 2, 3, 2); l(b, 3, 2, 3, 13); l(b, 3, 13, 5, 13); },
    '\\' => |b: &mut [u8; 16]| { l(b, 1, 3, 6, 12); },
    ']' => |b: &mut [u8; 16]| { l(b, 2, 2, 4, 2); l(b, 4, 2, 4, 13); l(b, 4, 13, 2, 13); },
    '^' => |b: &mut [u8; 16]| { l(b, 2, 6, 3, 3); l(b, 3, 3, 5, 6); },
    '_' => |b: &mut [u8; 16]| { l(b, 1, 12, 6, 12); },
    '`' => |b: &mut [u8; 16]| { l(b, 2, 3, 4, 5); },
    'a' => |b: &mut [u8; 16]| { l(b, 2, 6, 5, 6); l(b, 5, 6, 5, 11); l(b, 5, 11, 2, 11); l(b, 2, 8, 5, 8); l(b, 2, 11, 2, 8); },
    'b' => |b: &mut [u8; 16]| { l(b, 2, 3, 2, 11); l(b, 2, 6, 5, 6); l(b, 5, 6, 5, 8); l(b, 5, 8, 2, 8); l(b, 2, 8, 5, 11); l(b, 5, 11, 2, 11); },
    'c' => |b: &mut [u8; 16]| { l(b, 5, 7, 3, 6); l(b, 3, 6, 2, 8); l(b, 2, 8, 2, 10); l(b, 2, 10, 3, 11); l(b, 3, 11, 5, 10); },
    'd' => |b: &mut [u8; 16]| { l(b, 5, 3, 5, 11); l(b, 5, 6, 2, 6); l(b, 2, 6, 2, 8); l(b, 2, 8, 5, 8); l(b, 2, 8, 2, 10); l(b, 2, 10, 5, 11); },
    'e' => |b: &mut [u8; 16]| { l(b, 2, 8, 5, 8); l(b, 5, 8, 5, 7); l(b, 5, 7, 3, 6); l(b, 3, 6, 2, 8); l(b, 2, 8, 2, 10); l(b, 2, 10, 3, 11); l(b, 3, 11, 5, 10); },
    'f' => |b: &mut [u8; 16]| { l(b, 5, 2, 3, 4); l(b, 3, 4, 3, 11); l(b, 2, 6, 5, 6); },
    'g' => |b: &mut [u8; 16]| { l(b, 5, 6, 5, 13); l(b, 5, 13, 3, 14); l(b, 3, 14, 2, 13); l(b, 5, 6, 2, 6); l(b, 2, 6, 2, 8); l(b, 2, 8, 5, 8); },
    'h' => |b: &mut [u8; 16]| { l(b, 2, 3, 2, 11); l(b, 2, 6, 5, 6); l(b, 5, 6, 5, 11); },
    'i' => |b: &mut [u8; 16]| { l(b, 3, 3, 3, 3); l(b, 3, 6, 3, 11); l(b, 2, 11, 4, 11); },
    'j' => |b: &mut [u8; 16]| { l(b, 4, 3, 4, 3); l(b, 4, 6, 4, 13); l(b, 4, 13, 2, 14); },
    'k' => |b: &mut [u8; 16]| { l(b, 2, 3, 2, 11); l(b, 5, 6, 2, 9); l(b, 2, 9, 5, 11); },
    'l' => |b: &mut [u8; 16]| { l(b, 2, 3, 2, 11); l(b, 2, 11, 4, 11); },
    'm' => |b: &mut [u8; 16]| { l(b, 1, 11, 1, 6); l(b, 1, 6, 3, 6); l(b, 3, 6, 3, 11); l(b, 3, 6, 5, 6); l(b, 5, 6, 5, 11); },
    'n' => |b: &mut [u8; 16]| { l(b, 2, 11, 2, 6); l(b, 2, 6, 5, 6); l(b, 5, 6, 5, 11); },
    'o' => |b: &mut [u8; 16]| { l(b, 3, 6, 2, 8); l(b, 2, 8, 2, 10); l(b, 2, 10, 3, 11); l(b, 3, 11, 5, 11); l(b, 5, 11, 5, 8); l(b, 5, 8, 3, 6); },
    'p' => |b: &mut [u8; 16]| { l(b, 2, 6, 2, 14); l(b, 2, 6, 5, 6); l(b, 5, 6, 5, 8); l(b, 5, 8, 2, 8); l(b, 2, 8, 5, 11); l(b, 5, 11, 2, 11); },
    'q' => |b: &mut [u8; 16]| { l(b, 5, 6, 5, 14); l(b, 5, 6, 2, 6); l(b, 2, 6, 2, 8); l(b, 2, 8, 5, 8); l(b, 2, 8, 2, 11); l(b, 2, 11, 5, 11); },
    'r' => |b: &mut [u8; 16]| { l(b, 2, 11, 2, 6); l(b, 2, 6, 5, 6); l(b, 5, 6, 4, 8); },
    's' => |b: &mut [u8; 16]| { l(b, 5, 7, 3, 6); l(b, 3, 6, 2, 7); l(b, 2, 7, 5, 9); l(b, 5, 9, 5, 10); l(b, 5, 10, 3, 11); l(b, 3, 11, 2, 10); },
    't' => |b: &mut [u8; 16]| { l(b, 3, 3, 3, 11); l(b, 2, 6, 5, 6); },
    'u' => |b: &mut [u8; 16]| { l(b, 2, 6, 2, 10); l(b, 2, 10, 3, 11); l(b, 3, 11, 5, 11); l(b, 5, 11, 5, 6); },
    'v' => |b: &mut [u8; 16]| { l(b, 2, 6, 3, 11); l(b, 3, 11, 5, 6); },
    'w' => |b: &mut [u8; 16]| { l(b, 1, 6, 2, 11); l(b, 2, 11, 3, 8); l(b, 3, 8, 5, 11); l(b, 5, 11, 6, 6); },
    'x' => |b: &mut [u8; 16]| { l(b, 2, 6, 5, 11); l(b, 5, 6, 2, 11); },
    'y' => |b: &mut [u8; 16]| { l(b, 2, 6, 3, 11); l(b, 5, 6, 3, 9); l(b, 3, 9, 2, 13); },
    'z' => |b: &mut [u8; 16]| { l(b, 2, 6, 5, 6); l(b, 5, 6, 2, 11); l(b, 2, 11, 5, 11); },
    '{' => |b: &mut [u8; 16]| { l(b, 5, 2, 3, 5); l(b, 3, 5, 3, 9); l(b, 3, 9, 5, 12); l(b, 3, 7, 1, 7); },
    '|' => |b: &mut [u8; 16]| { l(b, 3, 2, 3, 13); },
    '}' => |b: &mut [u8; 16]| { l(b, 2, 2, 4, 5); l(b, 4, 5, 4, 9); l(b, 4, 9, 2, 12); l(b, 4, 7, 6, 7); },
    '~' => |b: &mut [u8; 16]| { l(b, 1, 8, 3, 6); l(b, 3, 6, 5, 9); l(b, 5, 9, 6, 7); },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec::Vec;

    #[test]
    fn table_covers_printable_ascii() {
        assert_eq!(font_table().len(), 95);
        for c in FIRST_CHAR..=LAST_CHAR {
            let _ = glyph(c);
        }
    }

    #[test]
    fn table_order_matches_char_order() {
        // A table built in the wrong order would render 'A' wherever '!' goes.
        assert_eq!(glyph(' '), [0u8; 16], "space must be blank");
        assert_ne!(glyph('A'), glyph('B'));
        // 'a' and 'A' are different glyphs in this font.
        assert_ne!(glyph('a'), glyph('A'));
        assert_ne!(glyph('0'), glyph('O'));
    }

    #[test]
    fn every_printable_glyph_has_ink() {
        // Space is the only legitimately blank glyph.
        for c in FIRST_CHAR..=LAST_CHAR {
            if c == ' ' {
                continue;
            }
            let g = glyph(c);
            assert!(g.iter().any(|r| *r != 0), "glyph {c:?} is completely blank");
        }
    }

    #[test]
    fn out_of_range_characters_fall_back_to_question_mark() {
        assert_eq!(glyph('\u{1F600}'), glyph('?'));
        assert_eq!(glyph('\u{0}'), glyph('?'));
        assert_eq!(glyph('\u{10FFFF}'), glyph('?'));
        assert_eq!(glyph('é'), glyph('?'), "non-ASCII is not in the table");
    }

    #[test]
    fn glyph_geometry_is_the_documented_size() {
        assert_eq!(GLYPH_WIDTH, 8);
        assert_eq!(GLYPH_HEIGHT, 16);
        assert_eq!(glyph('A').len(), 16);
    }

    #[test]
    fn text_width_accounts_for_scale() {
        assert_eq!(text_width("", 1), 0);
        assert_eq!(text_width("A", 1), 8);
        assert_eq!(text_width("A", 2), 16);
        assert_eq!(text_width("hello", 1), 40);
        // Scale 0 is clamped to 1 rather than collapsing all text to zero width.
        assert_eq!(text_width("A", 0), 8);
    }

    #[test]
    fn glyph_info_carries_the_advance() {
        let info = glyph_info('W');
        assert_eq!(info.advance, 8);
        assert_eq!(info.bitmap, glyph('W'));
    }

    #[test]
    fn digits_are_visually_distinct() {
        // A font table bug that repeats digits would make a serial log
        // unreadable, and that is exactly where this font is used first.
        let digits: Vec<Glyph> = ('0'..='9').map(glyph).collect();
        for i in 0..digits.len() {
            for j in (i + 1)..digits.len() {
                assert_ne!(digits[i], digits[j], "digits {i} and {j} are identical");
            }
        }
    }

    #[test]
    fn uppercase_letters_are_visually_distinct() {
        let letters: Vec<Glyph> = ('A'..='Z').map(glyph).collect();
        for i in 0..letters.len() {
            for j in (i + 1)..letters.len() {
                assert_ne!(letters[i], letters[j], "letters {i} and {j} are identical");
            }
        }
    }
}
