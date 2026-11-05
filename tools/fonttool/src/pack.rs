//! The `.velafont` writer: outlines in, one self-contained blob out.
//!
//! The byte layout is specified in `graphics/font/src/packed.rs`; this
//! module is its counterpart. The two are cross-checked by the round-trip
//! test below, which packs a synthetic font and reads it back with the
//! runtime's own parser — so the writer and reader cannot drift apart
//! without a test failing.

/// One glyph ready for packing: metrics and contours in font units.
pub struct PackedGlyph {
    /// Horizontal advance, in font units.
    pub advance: i32,
    /// One vector per closed contour; points carry their on-curve flag.
    pub contours: Vec<Vec<(i32, i32, bool)>>,
}

/// Pack a font into the `.velafont` byte format.
///
/// `map` pairs codepoints with glyph indices into `glyphs` and must be
/// sorted by codepoint (the runtime's charmap lookup is binary). Coordinates
/// saturate into the format's `i16` range, which real typefaces' ems (1000,
/// 2048) sit far inside.
pub fn pack(
    units_per_em: u16,
    ascender: i16,
    descender: i16,
    line_gap: i16,
    default_glyph: u16,
    map: &[(u32, u16)],
    glyphs: &[PackedGlyph],
) -> Vec<u8> {
    assert!(
        default_glyph < glyphs.len() as u16,
        "default glyph out of range"
    );

    let mut out = Vec::with_capacity(64 + glyphs.len() * 32);
    out.extend_from_slice(b"VLF1");
    out.extend_from_slice(&units_per_em.to_le_bytes());
    out.extend_from_slice(&ascender.to_le_bytes());
    out.extend_from_slice(&descender.to_le_bytes());
    out.extend_from_slice(&line_gap.to_le_bytes());
    out.extend_from_slice(&(glyphs.len() as u16).to_le_bytes());
    out.extend_from_slice(&(map.len() as u16).to_le_bytes());
    out.extend_from_slice(&default_glyph.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes()); // reserved

    // The charmap, already sorted by contract; enforce it here so a caller
    // bug becomes a tool error rather than a runtime misread.
    for window in map.windows(2) {
        assert!(window[0].0 < window[1].0, "charmap must be sorted");
    }
    for &(cp, glyph) in map {
        out.extend_from_slice(&cp.to_le_bytes());
        out.extend_from_slice(&glyph.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
    }

    // The directory: one placeholder per glyph plus the end sentinel.
    let dir_at = out.len();
    for _ in 0..=glyphs.len() {
        out.extend_from_slice(&0u32.to_le_bytes());
    }
    let glyphs_base = out.len();

    for (index, glyph) in glyphs.iter().enumerate() {
        let record_at = out.len();
        let at = dir_at + index * 4;
        out[at..at + 4].copy_from_slice(&((record_at - glyphs_base) as u32).to_le_bytes());
        pack_glyph(glyph, &mut out);
    }
    let end = out.len();
    let at = dir_at + glyphs.len() * 4;
    out[at..at + 4].copy_from_slice(&((end - glyphs_base) as u32).to_le_bytes());

    out
}

/// One glyph's record, appended to `out`.
fn pack_glyph(glyph: &PackedGlyph, out: &mut Vec<u8>) {
    let points: Vec<(i32, i32, bool)> = glyph.contours.iter().flatten().copied().collect();

    // The bounding box is computed here rather than trusted from the input,
    // because the runtime renders against the flattened points and a stale
    // box would clip ink.
    let mut x_min = i32::MAX;
    let mut x_max = i32::MIN;
    let mut y_min = i32::MAX;
    let mut y_max = i32::MIN;
    for &(x, y, _) in &points {
        x_min = x_min.min(x);
        x_max = x_max.max(x);
        y_min = y_min.min(y);
        y_max = y_max.max(y);
    }
    if points.is_empty() {
        (x_min, y_min, x_max, y_max) = (0, 0, 0, 0);
    }

    out.extend_from_slice(&(glyph.advance as i16).to_le_bytes());
    out.extend_from_slice(&(x_min as i16).to_le_bytes());
    out.extend_from_slice(&(y_min as i16).to_le_bytes());
    out.extend_from_slice(&(x_max as i16).to_le_bytes());
    out.extend_from_slice(&(y_max as i16).to_le_bytes());
    out.extend_from_slice(&(glyph.contours.len() as u16).to_le_bytes());
    out.extend_from_slice(&(points.len() as u16).to_le_bytes());
    // Contour end indices, cumulative over the contours' point counts.
    let mut end = 0u16;
    for contour in &glyph.contours {
        end += contour.len() as u16;
        out.extend_from_slice(&end.wrapping_sub(1).to_le_bytes());
    }
    for &(x, _, _) in &points {
        out.extend_from_slice(&(x as i16).to_le_bytes());
    }
    for &(_, y, _) in &points {
        out.extend_from_slice(&(y as i16).to_le_bytes());
    }
    // Flags: 2 bits per point, bit 0 of each pair set when on-curve.
    let mut flag_byte = 0u8;
    let mut filled = 0u32;
    for &(_, _, on) in &points {
        if on {
            flag_byte |= 1 << (filled % 4 * 2);
        }
        filled += 1;
        if filled % 4 == 0 {
            out.push(flag_byte);
            flag_byte = 0;
        }
    }
    if filled % 4 != 0 {
        out.push(flag_byte);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vela_font::Font;

    /// A unit square with a quadratic bulge on one side — every format
    /// feature the runtime reads, in one glyph.
    fn square_with_bulge() -> PackedGlyph {
        PackedGlyph {
            advance: 600,
            contours: vec![vec![
                (0, 0, true),
                (500, 0, true),
                (500, 200, true),
                // A quadratic corner: off-curve control pulling toward (300, 500).
                (500, 400, false),
                (300, 500, false),
                (100, 500, true),
                (0, 300, true),
            ]],
        }
    }

    #[test]
    fn a_packed_blob_round_trips_through_the_runtime_reader() {
        let map = vec![(0x41, 0u16), (0x42, 1u16)];
        let glyphs = vec![
            square_with_bulge(),
            PackedGlyph {
                advance: 250,
                contours: vec![],
            },
        ];
        let blob = pack(1000, 800, -200, 0, 0, &map, &glyphs);

        let font = Font::parse(&blob).expect("the tool's own blob must parse");
        assert_eq!(font.units_per_em(), 1000);
        assert_eq!(font.ascender(), 800);
        assert_eq!(font.descender(), -200);

        let a = font.glyph('A').expect("A is in the charmap");
        assert_eq!(a.advance_units(), 600);
        let b = font.glyph('B').expect("B is in the charmap");
        assert_eq!(b.advance_units(), 250);
        assert!(b.render(16).is_none(), "a blank glyph renders nothing");

        // Unmapped characters fall back to the default glyph, which is A.
        assert_eq!(font.glyph_or_default('§').advance_units(), 600);
    }

    #[test]
    fn the_rendered_area_matches_the_outline_area() {
        // The square body is 500×400 units; the bulge adds roughly a third
        // of a 200×400 rectangle. At 32 px with a 1000-unit em, the scale
        // is 32/1000, so the coverage total should sit near the analytic
        // value — far enough apart that a wrong fill rule or a sign error
        // in the winding cannot pass.
        let blob = pack(
            1000,
            800,
            -200,
            0,
            0,
            &[(0x41, 0u16)],
            &[square_with_bulge()],
        );
        let font = Font::parse(&blob).unwrap();
        let size = 32;
        let glyph = font.glyph('A').unwrap();
        let raster = glyph.render(size).expect("the square has ink");

        let total: u64 = raster.coverage.iter().map(|&c| c as u64).sum();
        let scale = size as f64 / 1000.0;
        let body = 500.0 * 400.0;
        // The implied-midpoint bulge: a quadratic from (500,400) to
        // (100,500) through (500,400)... its exact area is the triangle
        // fan the flattener cuts; the analytic bound below is loose.
        // Coverage sums 256 levels, so the pixel-area bounds scale by it.
        let expected_min = (body * 0.90) * scale * scale * 255.0;
        let expected_max = (body * 1.40) * scale * scale * 255.0;
        let actual = total as f64;
        assert!(
            actual >= expected_min && actual <= expected_max,
            "coverage {actual} outside [{expected_min}, {expected_max}]"
        );
    }

    #[test]
    fn a_truncated_blob_is_rejected_not_panicked_on() {
        let blob = pack(
            1000,
            800,
            -200,
            0,
            0,
            &[(0x41, 0u16)],
            &[square_with_bulge()],
        );
        for cut in [0, 4, 20, 30, blob.len() / 2] {
            assert!(
                Font::parse(&blob[..cut]).is_none(),
                "a {cut}-byte prefix parsed"
            );
        }
        // Flipping the magic breaks it too.
        let mut bad = blob.clone();
        bad[0] = b'X';
        assert!(Font::parse(&bad).is_none());
    }
}
