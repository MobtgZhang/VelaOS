//! Integer trigonometry and square roots for the desktop.
//!
//! # Why this exists
//!
//! The desktop is a bare-metal program. Its target has no maths library:
//! `f32::sin`, `f32::cos` and `f64::sqrt` are all `std` functions that
//! `core` does not provide, so a call to any of them fails to compile for
//! `x86_64-unknown-none`. The desktop draws an analogue clock, which needs
//! exactly those three functions.
//!
//! So they are implemented here, in integers.
//!
//! # The sine table
//!
//! The table holds the sine of every degree from 0 to 90 inclusive, in
//! Q16 fixed point — the value multiplied by 65536. A quarter circle is
//! enough, because sine is symmetric: the sine of an angle in the second
//! quadrant is the sine of its supplement, and the sine of an angle in
//! the third or fourth quadrant is negative.
//!
//! The table's values were computed with a maths library and rounded to
//! the nearest integer, so the largest error against the exact sine is
//! under one part in 65536 — a fraction of a pixel on a 46-pixel clock.
//!
//! # The square root
//!
//! Newton's method, on integers. It converges in a handful of iterations
//! for any value that fits in a `u32`, and the iteration count is bounded
//! so that a pathological input cannot spin.

/// The sine of every degree from 0 to 90, in Q16 fixed point.
///
/// The sine of 30 degrees is 32768 (0.5 in Q16), and the sine of 90
/// degrees is 65536 (1.0 in Q16).
const SIN_TABLE: [i32; 91] = [
    0, 1144, 2287, 3430, 4572, 5712, 6850, 7987, 9121, 10252, 11380, 12505, 13626, 14742, 15855,
    16962, 18064, 19161, 20252, 21336, 22415, 23486, 24550, 25607, 26656, 27697, 28729, 29753,
    30767, 31772, 32768, 33754, 34729, 35693, 36647, 37590, 38521, 39441, 40348, 41243, 42126,
    42995, 43852, 44695, 45525, 46341, 47143, 47930, 48703, 49461, 50203, 50931, 51643, 52339,
    53020, 53684, 54332, 54963, 55578, 56175, 56756, 57319, 57865, 58393, 58903, 59396, 59870,
    60326, 60764, 61183, 61584, 61966, 62328, 62672, 62997, 63303, 63589, 63856, 64104, 64332,
    64540, 64729, 64898, 65048, 65177, 65287, 65376, 65446, 65496, 65526, 65536,
];

/// The sine of an angle, in Q16 fixed point.
///
/// The angle is in **tenths of a degree**: 3600 is a full circle, and
/// 900 is a right angle. Tenths rather than whole degrees because a
/// clock's hour hand creeps half a degree per minute, and a whole-degree
/// table would make it jump.
pub fn sin_deci(deci: i32) -> i32 {
    // Reduce to [0, 3600). `rem_euclid` is what makes a negative angle
    // work: Rust's remainder keeps the sign of the dividend, so a
    // negative angle stays negative, and the sine of that would come out
    // with the wrong sign.
    let angle = deci.rem_euclid(3600);

    // The sine is positive in the first and second quadrants and negative
    // in the third and fourth, and the reflection about 90 degrees maps
    // the second quadrant onto the first.
    let (reduced, negative) = match angle {
        0..=900 => (angle, false),
        901..=1800 => (1800 - angle, false),
        1801..=2700 => (angle - 1800, true),
        _ => (3600 - angle, true),
    };

    let value = sine_first_quadrant(reduced);
    if negative {
        -value
    } else {
        value
    }
}

/// The cosine of an angle, in Q16 fixed point.
///
/// The angle is in tenths of a degree, as for [`sin_deci`]. The cosine of
/// an angle is the sine of its complement, so this reuses the same table
/// rather than carrying a second one.
pub fn cos_deci(deci: i32) -> i32 {
    sin_deci(900 - deci)
}

/// The sine of an angle in the first quadrant, in Q16 fixed point.
///
/// The angle is in tenths of a degree and must be between 0 and 900
/// inclusive; [`sin_deci`] is what reduces an arbitrary angle into that
/// range.
fn sine_first_quadrant(deci: i32) -> i32 {
    let whole = (deci / 10) as usize;
    let tenth = deci % 10;
    // The table's last entry is the sine of 90 degrees, so an angle of
    // exactly 90 must not index past it.
    if whole >= 90 {
        return SIN_TABLE[90];
    }
    // Interpolate between the two neighbouring entries, so the value
    // varies smoothly with the angle rather than stepping once a degree.
    let low = SIN_TABLE[whole];
    let high = SIN_TABLE[whole + 1];
    low + ((high - low) * tenth) / 10
}

/// The integer square root of `value`.
///
/// The largest `r` with `r * r <= value`. Newton's method, with the
/// iteration count bounded: the method converges quickly, and a bound
/// means a pathological input costs a fixed number of iterations rather
/// than an unbounded number.
pub fn isqrt(value: u32) -> u32 {
    if value == 0 {
        return 0;
    }
    // Newton's method, `next = (guess + value / guess) / 2`, started from
    // a guess of one above the true root and walking down. It converges
    // quadratically, so five or six iterations cover a 32-bit input.
    let mut guess = value;
    let mut next = guess / 2 + 1;
    // Twelve iterations is comfortably more than the convergence needs;
    // the bound is what makes the loop's cost independent of the input.
    for _ in 0..12 {
        if next >= guess {
            break;
        }
        guess = next;
        next = (guess + value / guess) / 2;
    }
    // Newton's method can land one above the true root, so step down while
    // the square still exceeds the value.
    while guess > 0 && guess.saturating_mul(guess) > value {
        guess -= 1;
    }
    guess
}
