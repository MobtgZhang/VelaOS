//! VelaOS's own classic palette and UI metrics.
//!
//! # Provenance
//!
//! Every value here is original to VelaOS. The *intent* is to evoke the general
//! feel of a late-80s/early-90s workstation desktop — flat greys, a blue accent,
//! chunky bevels, a 2-pixel double border — because that is what the plan calls
//! for, but no palette entry, coordinate, or gadget drawing is copied from
//! AmigaOS, AROS, Workbench or any other product. Nothing in this file is
//! derived from original artwork or bitmap resources.
//!
//! The palette deliberately avoids the exact greys of any historic system so
//! that "inspired by" stays true rather than becoming a copy.

// Linked into the kernel image, so `no_std`.
#![no_std]

use vela_renderer2d::Color;

/// Desktop backdrop.
pub const SCREEN_BG: Color = Color::rgb(0x3F, 0x50, 0x6B);
/// Window and panel fill.
pub const PANEL_BG: Color = Color::rgb(0xB5, 0xB5, 0xB5);
/// Window outline.
pub const PANEL_BORDER: Color = Color::rgb(0x2A, 0x2A, 0x2A);
/// Raised bevel highlight.
pub const BEVEL_LIGHT: Color = Color::rgb(0xE8, 0xE8, 0xE8);
/// Recessed bevel shadow.
pub const BEVEL_DARK: Color = Color::rgb(0x7A, 0x7A, 0x7A);
/// Primary text on a light background.
pub const TEXT_FG: Color = Color::rgb(0x10, 0x10, 0x10);
/// Secondary text.
pub const TEXT_DIM: Color = Color::rgb(0x50, 0x50, 0x50);
/// Accent used for highlights, the logo mark and the progress fill.
pub const ACCENT: Color = Color::rgb(0x2E, 0x6C, 0xB4);
/// Empty progress track.
pub const TRACK_BG: Color = Color::rgb(0x70, 0x70, 0x70);
/// Selected icon background.
pub const SELECT_BG: Color = Color::rgb(0x4C, 0x7A, 0xC4);
/// Screen title bar.
pub const TITLEBAR_BG: Color = Color::rgb(0x9A, 0xA8, 0xC0);
/// Active window title bar.
pub const TITLEBAR_ACTIVE_BG: Color = Color::rgb(0x5C, 0x74, 0x9E);
/// Close gadget.
pub const CLOSE_GADGET: Color = Color::rgb(0xC0, 0x50, 0x40);

/// Pixel metrics for window furniture.
pub mod metrics {
    /// Standard window border thickness.
    pub const BORDER: u32 = 2;
    /// Standard window title bar height.
    pub const TITLEBAR_HEIGHT: u32 = 20;
    /// Menu bar height.
    pub const MENUBAR_HEIGHT: u32 = 20;
    /// Desktop icon cell width.
    pub const ICON_CELL_WIDTH: u32 = 80;
    /// Desktop icon cell height.
    pub const ICON_CELL_HEIGHT: u32 = 72;
    /// Grid padding around the desktop icon area.
    pub const GRID_PADDING: u32 = 12;
    /// Close gadget side length.
    pub const CLOSE_GADGET_SIZE: u32 = 14;
    /// Spacing between adjacent windows when cascaded.
    pub const CASCADE_STEP: i32 = 24;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metrics_are_non_zero_and_ordered() {
        const {
            assert!(metrics::BORDER >= 1);
            assert!(metrics::TITLEBAR_HEIGHT > metrics::BORDER);
            assert!(metrics::MENUBAR_HEIGHT > 0);
            assert!(metrics::ICON_CELL_WIDTH > 0 && metrics::ICON_CELL_HEIGHT > 0);
            assert!(metrics::CLOSE_GADGET_SIZE > 0);
        }
    }

    #[test]
    fn palette_entries_are_distinct() {
        // A theme with two identical "different" colours is a bug that shows up
        // as an invisible border much later.
        let all = [
            SCREEN_BG,
            PANEL_BG,
            PANEL_BORDER,
            BEVEL_LIGHT,
            BEVEL_DARK,
            TEXT_FG,
            TEXT_DIM,
            ACCENT,
            TRACK_BG,
            SELECT_BG,
            TITLEBAR_BG,
            TITLEBAR_ACTIVE_BG,
            CLOSE_GADGET,
        ];
        for i in 0..all.len() {
            for j in (i + 1)..all.len() {
                assert_ne!(all[i], all[j], "duplicate palette entry at {i} and {j}");
            }
        }
    }

    #[test]
    fn text_has_contrast_against_panels() {
        // Crude luminance check: body text must not be the same lightness as
        // the fill it sits on, or labels will be unreadable.
        let luma = |c: Color| c.r as u32 + c.g as u32 + c.b as u32;
        assert!(luma(TEXT_FG) + 120 < luma(PANEL_BG), "body text too dim");
        assert!(luma(TEXT_DIM) + 60 < luma(PANEL_BG), "dim text unreadable");
    }

    #[test]
    fn bevel_light_is_brighter_than_bevel_dark() {
        const {
            assert!(BEVEL_LIGHT.r > BEVEL_DARK.r);
            assert!(BEVEL_LIGHT.g > BEVEL_DARK.g);
            assert!(BEVEL_LIGHT.b > BEVEL_DARK.b);
        }
    }

    #[test]
    fn active_titlebar_is_distinguishable_from_inactive() {
        assert_ne!(TITLEBAR_ACTIVE_BG, TITLEBAR_BG);
    }
}
