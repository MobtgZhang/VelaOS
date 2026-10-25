//! The VelaOS boot splash.
//!
//! Drawn straight into the GOP framebuffer with no firmware services and no
//! borrowed artwork: the geometry and palette come from `vela_theme_classic`,
//! which is VelaOS's own. The shape is an homage to the general idea of a
//! classic boot screen — a centred logo block over a flat background with a
//! progress rule — without copying any specific product's artwork.

use vela_renderer2d::{Color, Surface};
use vela_theme_classic as theme;

/// Draw the splash.
///
/// # Safety contract
///
/// The caller must already have validated the framebuffer geometry; this
/// function only issues bounded writes through [`Surface`], which clips and
/// range-checks every coordinate.
pub fn draw(surface: &mut Surface<'_>) {
    let w = surface.width();
    let h = surface.height();

    surface.fill_rect(theme::SCREEN_BG, 0, 0, w, h);

    // Centred logo block.
    let box_w = (w / 3).clamp(120, 420);
    let box_h = (h / 6).clamp(80, 240);
    let bx = ((w - box_w) / 2) as i32;
    let by = ((h - box_h) / 2) as i32 - (h / 12) as i32;

    surface.fill_rect(theme::PANEL_BG, bx, by, box_w, box_h);
    surface.stroke_rect(theme::PANEL_BORDER, bx, by, box_w, box_h);

    // The mark: three horizontal bars, a nod to the era without being a copy.
    let bar_h = (box_h / 8).max(4);
    let gap = bar_h * 2;
    let bar_x = bx + (box_w / 8) as i32;
    let bar_w_max = box_w - box_w / 4;
    for (i, frac) in [1.0f32, 0.72, 0.44].into_iter().enumerate() {
        let bw = ((bar_w_max as f32 * frac) as u32).max(bar_h);
        let by_i = by + (box_h / 3) as i32 + (i as u32 * gap) as i32;
        surface.fill_rect(theme::ACCENT, bar_x, by_i, bw, bar_h);
    }

    // Title under the mark, using the loader's embedded font.
    let scale = 2u32;
    let tw = vela_text::text_width("VelaOS", scale);
    let tx = ((w - tw) / 2) as i32;
    let ty = by + box_h as i32 + (h / 16) as i32;
    surface.draw_text(theme::TEXT_FG, "VelaOS", tx, ty, scale);

    // Progress rule: a filled and an empty track.
    let track_w = (w / 3).clamp(120, 420);
    let track_x = ((w - track_w) / 2) as i32;
    let track_y = ty + 24 * scale as i32;
    surface.fill_rect(theme::TRACK_BG, track_x, track_y, track_w, 6);
    surface.fill_rect(theme::ACCENT, track_x, track_y, track_w / 4, 6);

    let status = "Loading kernel";
    let sw = vela_text::text_width(status, 1);
    surface.draw_text(
        theme::TEXT_DIM,
        status,
        ((w - sw) / 2) as i32,
        track_y + 20,
        1,
    );

    // A version stamp in the corner: useful when comparing screenshots, and
    // it proves the embedded font is rendering in the loader, not the kernel.
    let ver = vela_boot_protocol::KERNEL_VERSION;
    let vw = vela_text::text_width(ver, 1);
    surface.draw_text(Color::rgb(0xE0, 0xE6, 0xF0), ver, (w - vw - 8) as i32, 8, 1);
}
