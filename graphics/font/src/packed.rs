//! The `.velafont` packed format: a validated reader for outline fonts.
//!
//! # The format, byte for byte (little-endian throughout)
//!
//! ```text
//! Header (20 bytes):
//!   0   4   magic "VLF1"
//!   4   2   units_per_em   u16  > 0
//!   6   2   ascender       i16  font units above the baseline
//!   8   2   descender      i16  font units below the baseline, ≤ 0
//!   10  2   line_gap       i16
//!   12  2   glyph_count    u16
//!   14  2   map_count      u16
//!   16  2   default_glyph  u16  index used for unmapped characters
//!   18  2   reserved       u16  zero
//!
//! Charmap (map_count × 8 bytes), sorted by codepoint ascending:
//!   u32 codepoint, u16 glyph_index, u16 zero
//!
//! Glyph directory ((glyph_count + 1) × u32): byte offsets of each glyph's
//! record relative to the end of the directory; the extra entry is the end
//! sentinel, so a record's length is a subtraction.
//!
//! Glyph record:
//!   i16 advance
//!   i16 x_min, y_min, x_max, y_max   (the outline's bounding box)
//!   u16 contour_count
//!   u16 point_count
//!   contour_count × u16  end-point index of each contour (ascending)
//!   point_count × i16    x coordinates, font units
//!   point_count × i16    y coordinates, font units
//!   ⌈point_count / 4⌉ × u8   flags, 2 bits per point; bit 0 set = on-curve
//! ```
//!
//! # Why font units, not pixels
//!
//! A blob that stored rendered pixels would be a bitmap font with extra
//! steps. Storing the outline in the typeface's own units means one blob
//! renders at any size, and the rasteriser ([`crate::raster`]) owns every
//! scaling decision.

/// The blob's magic, "VLF1" in ASCII.
pub const MAGIC: [u8; 4] = *b"VLF1";

/// The fixed-size prefix of every blob: the magic, seven header fields,
/// and one reserved word.
pub const HEADER_LEN: usize = 20;

/// The 8-byte charmap entry stride.
pub const MAP_ENTRY_LEN: usize = 8;

/// The 4-byte directory entry stride.
pub const DIR_ENTRY_LEN: usize = 4;

/// The parsed header: offsets into the blob plus the values callers need.
#[derive(Debug, Clone, Copy)]
pub struct Header {
    /// Font units per em.
    pub units_per_em: u16,
    /// Typographic ascender, in font units.
    pub ascender: i16,
    /// Typographic descender, in font units (≤ 0).
    pub descender: i16,
    /// Extra line spacing, in font units.
    pub line_gap: i16,
    /// How many glyphs the blob carries.
    pub glyph_count: u16,
    /// How many charmap entries there are.
    pub map_count: u16,
    /// The glyph substituted for unmapped characters.
    pub default_glyph: u16,
    /// Byte offset of the charmap.
    pub map_offset: usize,
    /// Byte offset of the glyph directory.
    pub dir_offset: usize,
    /// Byte offset of the glyph data.
    pub glyphs_offset: usize,
}

impl Header {
    /// Read and validate a blob header.
    ///
    /// Every check here is a corruption guard: a wrong magic, a zero
    /// units-per-em, or an offset past the blob each mean the bytes are not
    /// a font, and the honest answer is `None` rather than a panic three
    /// frames later.
    pub fn read(data: &[u8]) -> Option<Header> {
        if data.len() < HEADER_LEN {
            return None;
        }
        if data[0..4] != MAGIC {
            return None;
        }
        let units_per_em = u16_read(data, 4);
        if units_per_em == 0 {
            return None;
        }
        let ascender = i16_read(data, 6);
        let descender = i16_read(data, 8);
        let line_gap = i16_read(data, 10);
        let glyph_count = u16_read(data, 12);
        let map_count = u16_read(data, 14);
        let default_glyph = u16_read(data, 16);

        let map_offset = HEADER_LEN;
        let map_end = map_offset.checked_add(map_count as usize * MAP_ENTRY_LEN)?;
        let dir_offset = map_end;
        let dir_end = dir_offset.checked_add((glyph_count as usize + 1) * DIR_ENTRY_LEN)?;
        if dir_end > data.len() {
            return None;
        }
        let glyphs_offset = dir_end;
        // The directory's final entry is the end sentinel: the last glyph
        // must end inside the blob.
        let sentinel = dir_end - DIR_ENTRY_LEN;
        if u32_read(data, sentinel) as usize > data.len() {
            return None;
        }
        // The default glyph must name a real glyph; a blob whose default
        // points past the table would make every fallback render garbage.
        if default_glyph >= glyph_count {
            return None;
        }
        Some(Header {
            units_per_em,
            ascender,
            descender,
            line_gap,
            glyph_count,
            map_count,
            default_glyph,
            map_offset,
            dir_offset,
            glyphs_offset,
        })
    }

    /// The glyph index mapped to `ch`, or `None` when unmapped.
    ///
    /// The table is sorted, so the search is binary: a font with thousands
    /// of mappings still costs a handful of comparisons.
    pub fn charmap_lookup(&self, data: &[u8], ch: char) -> Option<u16> {
        let cp = ch as u32;
        let (mut lo, mut hi) = (0usize, self.map_count as usize);
        while lo < hi {
            let mid = (lo + hi) / 2;
            let entry = self.map_offset + mid * MAP_ENTRY_LEN;
            let key = u32_read(data, entry);
            if key < cp {
                lo = mid + 1;
            } else if key > cp {
                hi = mid;
            } else {
                return Some(u16_read(data, entry + 4));
            }
        }
        None
    }

    /// The byte range of glyph `index`'s record, from the directory.
    pub fn glyph_range(&self, data: &[u8], index: u16) -> Option<(usize, usize)> {
        let i = index as usize;
        if i >= self.glyph_count as usize {
            return None;
        }
        let start = self.dir_offset + i * DIR_ENTRY_LEN;
        let end = start + DIR_ENTRY_LEN;
        let from = u32_read(data, start) as usize;
        let to = u32_read(data, end) as usize;
        if from > to {
            return None;
        }
        let base = self.glyphs_offset;
        let from = base.checked_add(from)?;
        let to = base.checked_add(to)?;
        if to > data.len() {
            return None;
        }
        Some((from, to))
    }
}

/// One glyph's metrics and outline, read lazily on demand.
#[derive(Debug, Clone, Copy)]
pub struct GlyphRecord {
    /// Horizontal advance, in font units.
    pub advance: i32,
    /// Bounding box, in font units. A blank glyph's box is empty.
    pub x_min: i32,
    pub y_min: i32,
    pub x_max: i32,
    pub y_max: i32,
    /// How many closed contours the outline has.
    pub contours: usize,
    /// How many points the outline has in total.
    pub points: usize,
    /// Byte offset of the contour end indices, relative to the record start.
    ends_offset: usize,
    /// Byte offset of the x coordinates, relative to the record start.
    xs_offset: usize,
    /// Byte offset of the y coordinates, relative to the record start.
    ys_offset: usize,
    /// Byte offset of the flag bytes, relative to the record start.
    flags_offset: usize,
    /// Total record length, guarding every read below.
    len: usize,
    /// Absolute byte offset of the record inside the blob; the coordinate
    /// and flag offsets above are relative to it.
    start: usize,
}

impl GlyphRecord {
    /// Read glyph `index`'s record. A broken blob yields an advance-only
    /// (blank) glyph rather than a panic: the terminal keeps running.
    pub fn read(data: &[u8], header: &Header, index: u16) -> GlyphRecord {
        let blank = GlyphRecord {
            advance: 0,
            x_min: 0,
            y_min: 0,
            x_max: 0,
            y_max: 0,
            contours: 0,
            points: 0,
            ends_offset: 0,
            xs_offset: 0,
            ys_offset: 0,
            flags_offset: 0,
            len: 0,
            start: 0,
        };
        let Some((from, to)) = header.glyph_range(data, index) else {
            return blank;
        };
        // Two bytes of advance, eight of bounding box, and four of counts
        // come before the variable part.
        if to - from < 14 {
            return blank;
        }
        let rec = &data[from..to];
        let len = to - from;
        let advance = i16_read(rec, 0) as i32;
        let x_min = i16_read(rec, 2) as i32;
        let y_min = i16_read(rec, 4) as i32;
        let x_max = i16_read(rec, 6) as i32;
        let y_max = i16_read(rec, 8) as i32;
        let contours = u16_read(rec, 10) as usize;
        let points = u16_read(rec, 12) as usize;
        let ends_offset = 14usize;
        let xs_offset = ends_offset + contours * 2;
        let ys_offset = xs_offset + points * 2;
        let flags_offset = ys_offset + points * 2;
        let flags_end = flags_offset + points.div_ceil(4);
        if contours == 0 && points == 0 {
            // A blank glyph (a space): metrics only, nothing to read.
            return GlyphRecord {
                advance,
                x_min,
                y_min,
                x_max,
                y_max,
                contours: 0,
                points: 0,
                ends_offset: 0,
                xs_offset: 0,
                ys_offset: 0,
                flags_offset: 0,
                len,
                start: from,
            };
        }
        if flags_end > len {
            return blank;
        }
        GlyphRecord {
            advance,
            x_min,
            y_min,
            x_max,
            y_max,
            contours,
            points,
            ends_offset,
            xs_offset,
            ys_offset,
            flags_offset,
            len,
            start: from,
        }
    }

    /// The end-point index (inclusive) that closes contour `c`.
    ///
    /// Out of any range this returns `None`, and the outline walker stops —
    /// a truncated contour draws as much as it can rather than crashing.
    pub fn contour_end(&self, data: &[u8], c: usize) -> Option<u16> {
        if c >= self.contours {
            return None;
        }
        let at = self.start + self.ends_offset + c * 2;
        if at + 2 > self.start + self.len {
            return None;
        }
        Some(u16_read(data, at))
    }

    /// The x coordinate of point `i`, in font units.
    pub fn point_x(&self, data: &[u8], i: usize) -> Option<i16> {
        if i >= self.points {
            return None;
        }
        let at = self.start + self.xs_offset + i * 2;
        if at + 2 > self.start + self.len {
            return None;
        }
        Some(i16_read(data, at))
    }

    /// The y coordinate of point `i`, in font units.
    pub fn point_y(&self, data: &[u8], i: usize) -> Option<i16> {
        if i >= self.points {
            return None;
        }
        let at = self.start + self.ys_offset + i * 2;
        if at + 2 > self.start + self.len {
            return None;
        }
        Some(i16_read(data, at))
    }

    /// Whether point `i` sits on the curve (false = an off-curve control
    /// point of a quadratic Bézier).
    pub fn point_on_curve(&self, data: &[u8], i: usize) -> Option<bool> {
        if i >= self.points {
            return None;
        }
        let at = self.start + self.flags_offset + i / 4;
        if at >= self.start + self.len {
            return None;
        }
        Some((data[at] >> ((i % 4) * 2)) & 1 != 0)
    }

    /// Build the outline walker over this glyph's points.
    pub fn outline<'a>(&self, data: &'a [u8]) -> crate::raster::Outline<'a> {
        crate::raster::Outline {
            data,
            record: *self,
        }
    }
}

fn u16_read(data: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([data[at], data[at + 1]])
}

fn u32_read(data: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([data[at], data[at + 1], data[at + 2], data[at + 3]])
}

fn i16_read(data: &[u8], at: usize) -> i16 {
    i16::from_le_bytes([data[at], data[at + 1]])
}
