//! Scaling, flattening, and the antialiased rasteriser.
//!
//! # Pipeline
//!
//! 1. **Scale** — font-unit coordinates map to 26.6 fixed-point pixels
//!    (1/64 px per step, the same convention as FreeType), so sub-pixel
//!    geometry survives rounding.
//! 2. **Flatten** — TrueType outlines are quadratic Béziers, with the
//!    quirk that consecutive off-curve points carry an *implied* on-curve
//!    midpoint between them. [`Outline::flatten`] reconstructs those
//!    midpoints and cuts every quadratic into line segments whose maximum
//!    deviation from the true curve stays under a sub-pixel tolerance.
//! 3. **Rasterise** — [`rasterize`] walks each pixel row at 8 sub-scanlines,
//!    computes where every edge crosses, and applies the **non-zero winding
//!    rule** with 8 horizontal samples: 64 coverage levels, which is what
//!    makes diagonal edges antialiased instead of stair-stepped.

use alloc::vec;
use alloc::vec::Vec;
use core::cmp::{max, min};

use crate::packed::GlyphRecord;
use crate::{DivRound, RasterGlyph};

/// Sub-pixel flattening tolerance, in 26.6 units (here: 0.35 px).
const FLATTEN_TOL: i64 = 22;

/// Supersampling grid: 8×8 samples per pixel → 64 coverage levels.
const SUB: i64 = 8;

/// A scaled outline: closed contours of 26.6 fixed-point points.
///
/// A contour's last point connects back to its first implicitly, which is
/// one fewer entry to store — and one less chance to store it wrong.
pub struct Path {
    contours: Vec<Vec<(i64, i64)>>,
}

/// A glyph's outline, read straight out of a packed blob.
pub struct Outline<'a> {
    pub(crate) data: &'a [u8],
    pub(crate) record: GlyphRecord,
}

/// One outline point in font units plus its on-curve flag.
#[derive(Clone, Copy)]
struct UnitPoint {
    x: i64,
    y: i64,
    on_curve: bool,
}

impl Outline<'_> {
    /// Walk the glyph's contours in font units.
    ///
    /// A truncated or inconsistent record yields the contours it did
    /// describe rather than an error: a terminal draws what it can.
    fn unit_contours(&self) -> Vec<Vec<UnitPoint>> {
        let mut contours = Vec::new();
        let rec = &self.record;
        let data = self.data;
        let mut start_pt = 0usize;
        for c in 0..rec.contours {
            let Some(end) = rec.contour_end(data, c) else {
                break;
            };
            let end = end as usize;
            if end < start_pt || end >= rec.points {
                break;
            }
            let mut pts = Vec::new();
            let mut ok = true;
            for i in start_pt..=end {
                let (Some(x), Some(y), Some(on)) = (
                    rec.point_x(data, i),
                    rec.point_y(data, i),
                    rec.point_on_curve(data, i),
                ) else {
                    ok = false;
                    break;
                };
                pts.push(UnitPoint {
                    x: x as i64,
                    y: y as i64,
                    on_curve: on,
                });
            }
            if ok && !pts.is_empty() {
                contours.push(pts);
            }
            start_pt = end + 1;
        }
        contours
    }

    /// Scale to 26.6 pixels and flatten every quadratic to line segments.
    ///
    /// `sx`/`sy` are the 26.6 scale factors (pixels-per-em in 1/64 steps),
    /// computed by the caller from the requested pixel size.
    pub fn flatten(&self, sx: i64, sy: i64) -> Path {
        let mut contours = Vec::new();
        for unit in self.unit_contours() {
            let path = flatten_contour(&unit, sx, sy);
            if path.len() >= 3 {
                contours.push(path);
            }
        }
        Path { contours }
    }
}

/// Scale one font-unit point to 26.6 pixels.
fn scale(p: UnitPoint, sx: i64, sy: i64) -> (i64, i64) {
    (p.x * sx, p.y * sy)
}

/// Scale, insert implied midpoints, and flatten one contour.
fn flatten_contour(unit: &[UnitPoint], sx: i64, sy: i64) -> Vec<(i64, i64)> {
    // TrueType quirk: a contour may *start* with an off-curve point. The
    // implied on-curve point is the midpoint of the first and last points,
    // which closes the curve smoothly rather than with a corner.
    let starts_off = !unit[0].on_curve;
    let last_off = !unit[unit.len() - 1].on_curve;

    let mut pts: Vec<(i64, i64)> = Vec::with_capacity(unit.len() + 1);
    let mut on_curve: Vec<bool> = Vec::with_capacity(unit.len() + 1);
    for p in unit {
        pts.push(scale(*p, sx, sy));
        on_curve.push(p.on_curve);
    }
    if starts_off && last_off {
        let mid = (
            (pts[0].0 + pts[unit.len() - 1].0) / 2,
            (pts[0].1 + pts[unit.len() - 1].1) / 2,
        );
        pts.insert(0, mid);
        on_curve.insert(0, true);
    }

    let mut out: Vec<(i64, i64)> = Vec::new();
    // Walk the scaled points: `start` is the current segment's on-curve
    // start; buffered off-curve points accumulate until the next on-curve
    // point closes a quadratic (or a run of quadratics).
    let mut start: Option<(i64, i64)> = None;
    let mut pending: Vec<(i64, i64)> = Vec::new();
    for (i, &(x, y)) in pts.iter().enumerate() {
        let _ = i;
        if on_curve[p_index(&on_curve, i)] {
            let Some(cur) = start else {
                start = Some((x, y));
                out.push((x, y));
                continue;
            };
            emit_run(cur, &pending, (x, y), &mut out);
            start = Some((x, y));
            pending.clear();
        } else {
            pending.push((x, y));
        }
    }
    // Close the contour: a trailing run of off-curve points curves back to
    // the contour's first point.
    if let Some(cur) = start {
        if let Some(&first) = out.first() {
            emit_run(cur, &pending, first, &mut out);
        }
    }
    out
}

/// The on-curve flag walk runs over `on_curve` in lockstep with `pts`;
/// after the possible midpoint insertion the two vectors stay the same
/// length, so the index maps directly.
fn p_index(on_curve: &[bool], i: usize) -> usize {
    i.min(on_curve.len() - 1)
}

/// Emit the quadratics for an on-curve run from `cur` through `pending`
/// ending at `end`, appending interior flatten points plus `end`.
fn emit_run(cur: (i64, i64), pending: &[(i64, i64)], end: (i64, i64), out: &mut Vec<(i64, i64)>) {
    match pending.len() {
        0 => out.push(end),
        1 => flatten_quad(cur, pending[0], end, out),
        _ => {
            // Consecutive off-curve points imply an on-curve point at each
            // midpoint, splitting the run into one quadratic per pair.
            let mut prev_ctrl = pending[0];
            let mut cur = cur;
            for ctrl in &pending[1..] {
                let mid = ((prev_ctrl.0 + ctrl.0) / 2, (prev_ctrl.1 + ctrl.1) / 2);
                flatten_quad(cur, prev_ctrl, mid, out);
                out.push(mid);
                cur = mid;
                prev_ctrl = *ctrl;
            }
            flatten_quad(cur, prev_ctrl, end, out);
        }
    }
    out.push(end);
}

/// Flatten one quadratic Bézier into line segments, appending every
/// interior point (the caller appends the segment's endpoint).
fn flatten_quad(a: (i64, i64), c: (i64, i64), b: (i64, i64), out: &mut Vec<(i64, i64)>) {
    let dev = dist_chord(a, c, b);
    if dev <= FLATTEN_TOL {
        return; // Within tolerance of the chord: the chord *is* the curve.
    }
    // Bisecting a quadratic quarters its deviation, so n halvings bring a
    // deviation `dev` under tolerance when 4^n ≥ dev/tol. Computed by a
    // bounded loop rather than floating-point logs.
    let mut count = 0;
    let mut r = dev / FLATTEN_TOL;
    while r > 1 && count < 16 {
        r /= 4;
        count += 1;
    }
    let steps = 1i64 << count;
    // De Casteljau in 16.16 parameter arithmetic: the intermediate products
    // stay far inside i64, and the rounding error is a fraction of a
    // 1/64 px step.
    const FRAC: i64 = 1 << 16;
    for i in 1..steps {
        let t = i * (FRAC / steps);
        let p0 = lerp2(a, c, t, FRAC);
        let p1 = lerp2(c, b, t, FRAC);
        out.push(lerp2(p0, p1, t, FRAC));
    }
}

/// `a + (b - a) * t / frac`, with `frac` the parameter's fixed-point unit.
fn lerp2(a: (i64, i64), b: (i64, i64), t: i64, frac: i64) -> (i64, i64) {
    (
        a.0 + ((b.0 - a.0) * t).div_round(frac.max(1)),
        a.1 + ((b.1 - a.1) * t).div_round(frac.max(1)),
    )
}

/// Distance from `c` to the line through `a`—`b`, in 26.6 units.
///
/// All-integer: `|cross| / |chord| = √(cross² / chord²)`, and the inner
/// ratio is computed in i128 before an integer square root, so no float
/// intrinsics are needed in `no_std`.
fn dist_chord(a: (i64, i64), c: (i64, i64), b: (i64, i64)) -> i64 {
    let ax = (b.0 - a.0) as i128;
    let ay = (b.1 - a.1) as i128;
    let len2 = (ax * ax + ay * ay).max(1);
    let cross = ((c.0 - a.0) as i128 * ay - (c.1 - a.1) as i128 * ax).abs();
    isqrt((cross * cross / len2) as i64)
}

/// The square root of `n`, rounded down, by Newton's method.
fn isqrt(n: i64) -> i64 {
    if n <= 0 {
        return 0;
    }
    let mut x = n;
    let mut y = (x + 1) / 2;
    while y < x {
        x = y;
        y = (x + n / x) / 2;
    }
    x
}

/// Integer division rounded down, for negative numerators too.
fn floor_div(a: i64, b: i64) -> i64 {
    let q = a / b;
    if a % b != 0 && (a < 0) != (b < 0) {
        q - 1
    } else {
        q
    }
}

/// Integer division rounded up, for negative numerators too.
fn ceil_div(a: i64, b: i64) -> i64 {
    -floor_div(-a, b)
}

/// Rasterise a flattened path into an antialiased coverage bitmap.
///
/// The bitmap covers the path's bounding box, which the glyph's metrics
/// translate into blit offsets: [`RasterGlyph::left`] right of the pen and
/// [`RasterGlyph::top`] up from the baseline.
pub fn rasterize(path: &Path) -> Option<RasterGlyph> {
    // The bounding box, in whole pixels: floor of the minimum, *exclusive*
    // ceiling of the maximum. Pixel (px, py) covers
    // x ∈ [xmin + px, xmin + px + 1) and y ∈ [ymax - py - 1, ymax - py),
    // so row 0 is the bitmap's top and y grows downward on screen while
    // the outline's y grows upward from the baseline.
    let mut min_x = i64::MAX;
    let mut max_x = i64::MIN;
    let mut min_y = i64::MAX;
    let mut max_y = i64::MIN;
    for contour in &path.contours {
        for &(x, y) in contour {
            min_x = min(min_x, x);
            max_x = max(max_x, x);
            min_y = min(min_y, y);
            max_y = max(max_y, y);
        }
    }
    if min_x > max_x || min_y > max_y {
        return None;
    }
    let left = floor_div(min_x, 64);
    let right = ceil_div(max_x, 64);
    let top = ceil_div(max_y, 64);
    let bottom = floor_div(min_y, 64);
    let width = (right - left).max(1) as u32;
    let height = (top - bottom).max(1) as u32;

    // Coverage accumulates over the SUB sub-scanlines each pixel row is
    // divided into, then maps onto the 0..=255 output scale.
    let mut coverage = vec![0u8; (width as usize) * (height as usize)];
    let mut row_acc = vec![0i64; width as usize];
    let mut crossings: Vec<(i64, i64)> = Vec::new(); // (x in 26.6, winding ±1)

    for py in 0..height as i64 {
        row_acc.fill(0);
        for s in 0..SUB {
            // The sub-scanline's y, in 26.6, centred in its sub-cell.
            let y = (top - 1 - py) * 64 + s * (64 / SUB) + (64 / SUB) / 2;
            crossings.clear();
            for contour in &path.contours {
                raster_contour_row(contour, y, &mut crossings);
            }
            if crossings.is_empty() {
                continue;
            }
            crossings.sort_unstable_by_key(|&(x, _)| x);
            // Walk the pixel columns: each sub-sample counts the winding of
            // every crossing strictly left of it. The pointer walk relies
            // on `x` growing with `px`, which it does, and on `winding`
            // accumulating left to right, which it also does.
            let mut idx = 0usize;
            let mut winding: i64 = 0;
            for px in 0..width as i64 {
                for t in 0..SUB {
                    let x = (left + px) * 64 + t * (64 / SUB) + (64 / SUB) / 2;
                    while idx < crossings.len() && crossings[idx].0 < x {
                        winding += crossings[idx].1;
                        idx += 1;
                    }
                    if winding != 0 {
                        row_acc[px as usize] += 1;
                    }
                }
            }
        }
        for px in 0..width as usize {
            coverage[py as usize * width as usize + px] =
                ((row_acc[px] * 255 + SUB * SUB / 2) / (SUB * SUB)) as u8;
        }
    }
    Some(RasterGlyph {
        width,
        height,
        left: left as i32,
        top: top as i32,
        coverage,
    })
}

/// The crossings of one contour with one sub-scanline.
///
/// Edges are half-open in y — `[low, high)` — so a vertex shared by two
/// edges counts once, and a truly horizontal edge counts zero times. The
/// winding is +1 for edges going up the screen (increasing y) and −1 for
/// edges going down, which is what the non-zero rule needs.
fn raster_contour_row(contour: &[(i64, i64)], y: i64, out: &mut Vec<(i64, i64)>) {
    let n = contour.len();
    for i in 0..n {
        let (x1, y1) = contour[i];
        let (x2, y2) = contour[(i + 1) % n];
        let (dir, (ya, xa), (yb, xb)) = if y1 <= y2 {
            (1, (y1, x1), (y2, x2))
        } else {
            (-1, (y2, x2), (y1, x1))
        };
        if ya <= y && y < yb {
            // x at y, interpolated between the edge's lower and upper
            // endpoints; the intermediate product spans a few thousand
            // squared, well inside i64.
            let dy = (yb - ya).max(1);
            let x = xa + (xb - xa) * (y - ya) / dy;
            out.push((x, dir));
        }
    }
}
