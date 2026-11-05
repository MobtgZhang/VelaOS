//! A minimal but complete TrueType/TTC reader for the subsetter.
//!
//! # Scope
//!
//! Only what packing a `.velafont` needs, and nothing else: the table
//! directory, `head`/`maxp`/`hhea`/`hmtx` for metrics, `cmap` (formats 4 and
//! 12) for character mapping, and `loca`/`glyf` for outlines — including
//! composite glyphs, which are decomposed into their components here so the
//! runtime never has to know they existed.
//!
//! Hinting instructions are deliberately dropped: the rasteriser is an
//! antialiased scanline fill, which does its own scaling and needs none.

use std::collections::BTreeMap;
use std::fmt;

/// Everything that can go wrong, with the context a maintainer needs.
#[derive(Debug)]
pub struct Error(pub String);

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

pub type Result<T> = std::result::Result<T, Error>;

fn err<T>(msg: impl Into<String>) -> Result<T> {
    Err(Error(msg.into()))
}

macro_rules! need {
    ($cond:expr, $msg:expr) => {
        if !($cond) {
            return err($msg);
        }
    };
}

/// One glyph's outline in font units, fully decomposed.
#[derive(Debug, Clone, Default)]
pub struct Outline {
    /// One vector per closed contour; each point carries its on-curve flag.
    pub contours: Vec<Vec<(i32, i32, bool)>>,
}

/// One face inside a font file.
pub struct Face<'a> {
    data: &'a [u8],
    /// Table tag → (offset, length).
    tables: BTreeMap<[u8; 4], (u64, u64)>,
    num_glyphs: u16,
    units_per_em: u16,
    ascender: i16,
    descender: i16,
    line_gap: i16,
    num_h_metrics: u16,
    index_to_loc_format: u16,
    /// Sorted charmap from the best available subtable.
    charmap: BTreeMap<u32, u16>,
}

impl<'a> Face<'a> {
    /// Open face `index` of a `.ttf`/`.otf`/`.ttc` file.
    pub fn open(data: &'a [u8], index: usize) -> Result<Face<'a>> {
        need!(data.len() >= 12, "file is too short for any sfnt header");
        let base = match &data[0..4] {
            b"ttcf" => {
                // TrueType collection: a count, then one u32 offset per face.
                let count = u32(data, 8)? as usize;
                need!(
                    index < count,
                    format!("face {index} requested, collection has {count}")
                );
                let off = u32(data, 12 + index * 4)? as usize;
                need!(
                    off + 12 <= data.len(),
                    "collection face offset out of range"
                );
                off
            }
            // A bare sfnt starts with its own table directory version.
            _ => 0,
        };
        let tables = Self::table_directory(data, base)?;
        let face = Face {
            data,
            tables,
            num_glyphs: 0,
            units_per_em: 0,
            ascender: 0,
            descender: 0,
            line_gap: 0,
            num_h_metrics: 0,
            index_to_loc_format: 0,
            charmap: BTreeMap::new(),
        };
        let mut face = face;
        face.load_head()?;
        face.load_maxp()?;
        face.load_hhea()?;
        face.load_cmap()?;
        Ok(face)
    }

    /// The sfnt table directory: u16 count, then tag/checksum/offset/length.
    fn table_directory(data: &[u8], base: usize) -> Result<BTreeMap<[u8; 4], (u64, u64)>> {
        let n = u16(data, base + 4)? as usize;
        let mut tables = BTreeMap::new();
        for i in 0..n {
            let at = base + 12 + i * 16;
            need!(at + 16 <= data.len(), "table directory entry out of range");
            let tag = [data[at], data[at + 1], data[at + 2], data[at + 3]];
            let off = u32(data, at + 8)? as u64;
            let len = u32(data, at + 12)? as u64;
            if off + len > data.len() as u64 {
                return err(format!("table {tag:?} runs past the end of the file"));
            }
            tables.insert(tag, (off, len));
        }
        Ok(tables)
    }

    fn need_table(&self, tag: &[u8; 4]) -> Result<(u64, u64)> {
        self.tables
            .get(tag)
            .copied()
            .ok_or_else(|| Error(format!("missing table {:?}", *tag)))
    }

    fn load_head(&mut self) -> Result<()> {
        let (off, len) = self.need_table(b"head")?;
        need!(len >= 54, "head table too short");
        self.units_per_em = u16(self.data, off as usize + 18)?;
        // indexToLocFormat is a 16-bit field at offset 50; reading a single
        // byte would see format 1's high byte (0) and misread every loca.
        self.index_to_loc_format = u16(self.data, off as usize + 50)?;
        Ok(())
    }

    fn load_maxp(&mut self) -> Result<()> {
        let (off, _len) = self.need_table(b"maxp")?;
        self.num_glyphs = u16(self.data, off as usize + 4)?;
        Ok(())
    }

    fn load_hhea(&mut self) -> Result<()> {
        let (off, len) = self.need_table(b"hhea")?;
        need!(len >= 36, "hhea table too short");
        self.ascender = i16(self.data, off as usize + 4)?;
        self.descender = i16(self.data, off as usize + 6)?;
        self.line_gap = i16(self.data, off as usize + 8)?;
        self.num_h_metrics = u16(self.data, off as usize + 34)?;
        Ok(())
    }

    /// Build the sorted char→glyph map from the best cmap subtable.
    fn load_cmap(&mut self) -> Result<()> {
        let (base, len) = self.need_table(b"cmap")?;
        let n = u16(self.data, base as usize + 2)? as usize;
        let mut best: Option<(usize, u32)> = None; // (format, subtable offset)
        for i in 0..n {
            let at = base as usize + 4 + i * 8;
            let sub_off = base + u32(self.data, at + 4)? as u64;
            let format = u16(self.data, sub_off as usize)?;
            // Format 12 covers the whole Unicode range; format 4 covers the
            // BMP. Prefer the one that maps everything we might ask for.
            if format == 12 || (format == 4 && best.map(|(f, _)| f != 12).unwrap_or(true)) {
                need!(
                    sub_off + 4 <= len + base,
                    "cmap subtable offset out of range"
                );
                best = Some((format as usize, sub_off as u32));
            }
        }
        let Some((format, sub)) = best else {
            return err("no usable cmap subtable (need format 4 or 12)");
        };
        self.charmap = match format {
            12 => self.cmap12(sub as usize)?,
            _ => self.cmap4(sub as usize)?,
        };
        Ok(())
    }

    /// Unicode format 4: segment arrays with idDelta / idRangeOffset.
    fn cmap4(&self, at: usize) -> Result<BTreeMap<u32, u16>> {
        let mut map = BTreeMap::new();
        let seg_x2 = u16(self.data, at + 6)? as usize;
        let seg_count = seg_x2 / 2;
        let ends_at = at + 14;
        let starts_at = ends_at + seg_x2;
        let deltas_at = starts_at + seg_x2;
        let ranges_at = deltas_at + seg_x2;
        for s in 0..seg_count {
            let end = u16(self.data, ends_at + s * 2)? as u32;
            let start = u16(self.data, starts_at + s * 2)? as u32;
            if start == 0xFFFF {
                continue; // The sentinel segment maps nothing.
            }
            for cp in start..=end {
                let glyph = {
                    let range = u16(self.data, ranges_at + s * 2)? as usize;
                    if range == 0 {
                        let delta = i16(self.data, deltas_at + s * 2)?;
                        (cp as i64 + delta as i64) as u16
                    } else {
                        // The range offset points *relative to itself* to the
                        // id array, or to 0 for "map to 0".
                        let addr = ranges_at + s * 2 + range + ((cp - start) as usize) * 2;
                        need!(
                            addr + 2 <= self.data.len(),
                            "cmap4 range offset out of range"
                        );
                        let id = u16(self.data, addr)?;
                        id.wrapping_add(cp as u16)
                    }
                };
                if glyph != 0 {
                    map.insert(cp, glyph);
                }
            }
        }
        Ok(map)
    }

    /// Unicode format 12: a sorted list of (start, end, start-glyph) groups.
    fn cmap12(&self, at: usize) -> Result<BTreeMap<u32, u16>> {
        let mut map = BTreeMap::new();
        let groups = u32(self.data, at + 12)? as usize;
        for g in 0..groups {
            let rec = at + 16 + g * 12;
            need!(rec + 12 <= self.data.len(), "cmap12 group out of range");
            let start = u32(self.data, rec)?;
            let end = u32(self.data, rec + 4)?;
            let first = u32(self.data, rec + 8)?;
            for (i, cp) in (start..=end).enumerate() {
                map.insert(cp, first as u16 + i as u16);
            }
        }
        Ok(map)
    }

    /// The glyph a character maps to, or `None` for unmapped.
    pub fn glyph_for(&self, ch: char) -> Option<u16> {
        self.charmap.get(&(ch as u32)).copied()
    }

    /// The face's charmap, for building the blob's own.
    pub fn charmap(&self) -> &BTreeMap<u32, u16> {
        &self.charmap
    }

    pub fn units_per_em(&self) -> u16 {
        self.units_per_em
    }

    pub fn ascender(&self) -> i16 {
        self.ascender
    }

    pub fn descender(&self) -> i16 {
        self.descender
    }

    pub fn line_gap(&self) -> i16 {
        self.line_gap
    }

    /// The advance width of `glyph`, in font units.
    pub fn advance(&self, glyph: u16) -> Result<i32> {
        let (off, _len) = self.need_table(b"hmtx")?;
        let n = self.num_h_metrics as u64;
        let i = (glyph as u64).min(n.saturating_sub(1));
        let at = off as usize + i as usize * 4;
        need!(at + 2 <= self.data.len(), "hmtx entry out of range");
        Ok(u16(self.data, at)? as i32)
    }

    /// The glyph's outline, decomposed: composites resolved into the
    /// transformed outlines of their components.
    pub fn outline(&self, glyph: u16) -> Result<Outline> {
        self.outline_inner(glyph, 0)
    }

    fn outline_inner(&self, glyph: u16, depth: usize) -> Result<Outline> {
        if depth > 8 {
            return err("composite nesting deeper than 8 — refusing to recurse");
        }
        let (glyf_off, glyf_len) = self.need_table(b"glyf")?;
        let (loca_off, loca_len) = self.need_table(b"loca")?;
        need!(
            (glyph as usize) < self.num_glyphs as usize,
            format!("glyph {glyph} out of range")
        );
        // loca has numGlyphs+1 entries; entry widths follow indexToLocFormat.
        let (loca_at, entry_len) = match self.index_to_loc_format {
            0 => (loca_off as usize + glyph as usize * 2, 2usize),
            _ => (loca_off as usize + glyph as usize * 4, 4usize),
        };
        need!(
            loca_len as usize >= (glyph as usize + 1) * entry_len,
            "loca too short for glyph"
        );
        let (from, to) = match entry_len {
            2 => (
                u16(self.data, loca_at)? as usize * 2,
                u16(self.data, loca_at + 2)? as usize * 2,
            ),
            _ => (
                u32(self.data, loca_at)? as usize,
                u32(self.data, loca_at + 4)? as usize,
            ),
        };
        if from >= to {
            return Ok(Outline::default()); // An empty (blank) glyph.
        }
        let at = glyf_off as usize + from;
        need!(
            glyf_len as usize >= to - from && at + to - from <= self.data.len(),
            "glyf range out of bounds"
        );
        let rec = &self.data[at..at + (to - from)];
        let contours = i16(rec, 0)?;
        if contours >= 0 {
            self.simple_outline(rec)
        } else {
            self.composite_outline(rec, depth)
        }
    }

    /// A simple glyph: contour end points, then flags and delta coordinates.
    fn simple_outline(&self, rec: &[u8]) -> Result<Outline> {
        let n = i16(rec, 0)? as usize;
        let mut ends = Vec::with_capacity(n);
        let mut max_pt = 0usize;
        for i in 0..n {
            let end = u16(rec, 10 + i * 2)? as usize;
            max_pt = max_pt.max(end + 1);
            ends.push(end);
        }
        let instr_len = u16(rec, 10 + n * 2)? as usize;
        let mut at = 12 + n * 2 + instr_len;
        // Flags, with the repeat bit fanning one byte out into many points.
        let mut flags = Vec::with_capacity(max_pt);
        while flags.len() < max_pt {
            need!(at < rec.len(), "glyph flags run past the record");
            let f = rec[at];
            at += 1;
            flags.push(f);
            if f & 0x08 != 0 {
                need!(at < rec.len(), "flag repeat count runs past the record");
                let repeat = rec[at];
                at += 1;
                for _ in 0..repeat {
                    flags.push(f);
                }
            }
        }
        flags.truncate(max_pt);
        // Coordinates are delta-coded, one axis at a time, with each
        // point's width chosen by flag bits: the short bit makes the
        // delta one byte (unsigned; the same-or-positive bit picks its
        // sign), and without it the same-or-positive bit means "same as
        // the previous point". Otherwise the delta is a signed 16-bit
        // value. X uses bits 1/4, Y uses bits 2/5.
        let (xs, at_y) = decode_deltas(rec, at, &flags, 1, 4)?;
        let (ys, _at_end) = decode_deltas(rec, at_y, &flags, 2, 5)?;

        // Rebuild contours from the end-point list, carrying the on-curve bit.
        let mut contours = Vec::with_capacity(n);
        let mut start = 0usize;
        for end in ends {
            let mut contour = Vec::with_capacity(end + 1 - start);
            for i in start..=end {
                contour.push((xs[i], ys[i], flags[i] & 0x01 != 0));
            }
            contours.push(contour);
            start = end + 1;
        }
        Ok(Outline { contours })
    }

    /// A composite glyph: walk its components and transform each one.
    fn composite_outline(&self, rec: &[u8], depth: usize) -> Result<Outline> {
        let mut out = Outline::default();
        let mut at = 10usize; // Past the header, whose box we recompute.
        loop {
            need!(at + 4 <= rec.len(), "composite component header truncated");
            let flags = u16(rec, at)?;
            let component = u16(rec, at + 2)?;
            at += 4;
            let words = flags & 0x0001 != 0;
            let xy_values = flags & 0x0002 != 0;
            need!(
                xy_values,
                "composite component uses point matching, which the packer does not support"
            );
            let (dx, dy) = if words {
                need!(at + 4 <= rec.len(), "composite args truncated");
                let d = (i16(rec, at)? as i32, i16(rec, at + 2)? as i32);
                at += 4;
                d
            } else {
                need!(at + 2 <= rec.len(), "composite args truncated");
                let d = (rec[at] as i8 as i32, rec[at + 1] as i8 as i32);
                at += 2;
                d
            };
            // The transform defaults to identity and gains scale entries per
            // flag; F2Dot14 values are 16384ths.
            let mut m = [16384i32, 0, 0, 16384i32];
            if flags & 0x0008 != 0 {
                need!(at + 2 <= rec.len(), "composite scale truncated");
                m[0] = i16(rec, at)? as i32;
                m[3] = m[0];
                at += 2;
            } else if flags & 0x0040 != 0 {
                need!(at + 4 <= rec.len(), "composite scale truncated");
                m[0] = i16(rec, at)? as i32;
                m[3] = i16(rec, at + 2)? as i32;
                at += 4;
            } else if flags & 0x0080 != 0 {
                need!(at + 8 <= rec.len(), "composite matrix truncated");
                m[0] = i16(rec, at)? as i32;
                m[1] = i16(rec, at + 2)? as i32;
                m[2] = i16(rec, at + 4)? as i32;
                m[3] = i16(rec, at + 6)? as i32;
                at += 8;
            }
            let sub = self.outline_inner(component, depth + 1)?;
            for contour in sub.contours {
                let transformed: Vec<(i32, i32, bool)> = contour
                    .iter()
                    .map(|&(x, y, on)| {
                        // F2Dot14: divide by 16384 after multiplying.
                        let px = (m[0] * x + m[1] * y) / 16384 + dx;
                        let py = (m[2] * x + m[3] * y) / 16384 + dy;
                        (px, py, on)
                    })
                    .collect();
                out.contours.push(transformed);
            }
            if flags & 0x0020 == 0 {
                break; // No MORE_COMPONENTS flag: this was the last one.
            }
        }
        Ok(out)
    }
}

/// Decode one axis's delta-coded coordinates.
///
/// `short_bit` marks one-byte deltas, whose sign the `positive_bit`
/// carries; without the short bit, a set `positive_bit` means "same as
/// the previous point" and a clear one a signed 16-bit delta. Returns
/// the absolute coordinates and the offset just past the axis's data.
fn decode_deltas(
    rec: &[u8],
    mut at: usize,
    flags: &[u8],
    short_bit: u32,
    positive_bit: u32,
) -> Result<(Vec<i32>, usize)> {
    let short_mask = 1u8 << short_bit;
    let positive_mask = 1u8 << positive_bit;
    let mut out = Vec::with_capacity(flags.len());
    let mut value = 0i32;
    for &f in flags {
        if f & short_mask != 0 {
            need!(at < rec.len(), "coordinate deltas run past the record");
            // One byte, magnitude only: the same-or-positive bit picks the
            // sign, so the range is -255..=255, not a two's-complement byte.
            let delta = rec[at] as i32;
            at += 1;
            value += if f & positive_mask != 0 {
                delta
            } else {
                -delta
            };
        } else if f & positive_mask == 0 {
            need!(at + 2 <= rec.len(), "coordinate deltas run past the record");
            value += i16::from_be_bytes([rec[at], rec[at + 1]]) as i32;
            at += 2;
        }
        // A clear short bit with the positive bit set leaves `value` as
        // it was: the point repeats its predecessor's coordinate.
        out.push(value);
    }
    Ok((out, at))
}

fn u16(data: &[u8], at: usize) -> Result<u16> {
    need!(at + 2 <= data.len(), "read past end of file");
    Ok(u16::from_be_bytes([data[at], data[at + 1]]))
}

fn u32(data: &[u8], at: usize) -> Result<u32> {
    need!(at + 4 <= data.len(), "read past end of file");
    Ok(u32::from_be_bytes([
        data[at],
        data[at + 1],
        data[at + 2],
        data[at + 3],
    ]))
}

fn i16(data: &[u8], at: usize) -> Result<i16> {
    need!(at + 2 <= data.len(), "read past end of file");
    Ok(i16::from_be_bytes([data[at], data[at + 1]]))
}
