//! The desktop's icon artwork.
//!
//! # Where the artwork comes from
//!
//! These are **AROS's own icons**: the "Mason" icon set from the AROS
//! repository (`images/IconSets/Mason`), which is what the AROS desktop
//! draws its volume, drawer and program icons from. The AROS project
//! distributes them under its own licence, and [`NOTICE`] carries the
//! attribution and the terms.
//!
//! The icons are indexed-colour PNGs. They are converted once, by
//! `tools/icontool/convert.py`, into the compact form this crate reads: a
//! header, the palette, and a run-length encoded index plane. The kernel
//! has no inflate, and an icon plane of 46x46 indexes is a couple of
//! kilobytes run-length encoded — a fair price for being able to draw a
//! real AROS icon without a decoder in the boot path.
//!
//! # Drawing an icon
//!
//! An icon is a palette and an index per pixel, with one index marked
//! transparent. [`Icon::draw`] puts the icon's pixels into a surface,
//! skipping the transparent one, so the desktop's background shows through
//! where the artwork does not cover.

#![no_std]

extern crate alloc;

use alloc::vec::Vec;

use vela_renderer2d::{Color, Surface};

/// The magic at the head of a converted icon.
const MAGIC: &[u8; 4] = b"IDAT";
/// The format version this crate understands.
const VERSION: u8 = 1;

/// A decoded icon: a palette and an index plane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Icon {
    width: u16,
    height: u16,
    palette: Vec<Color>,
    pixels: Vec<u8>,
    transparent: Option<u8>,
}

impl Icon {
    /// Decode a converted icon.
    ///
    /// Returns `None` when the data is not an icon this crate understands.
    pub fn decode(raw: &[u8]) -> Option<Self> {
        if raw.len() < 11 || &raw[0..4] != MAGIC || raw[4] != VERSION {
            return None;
        }
        let width = u16::from_be_bytes([raw[5], raw[6]]);
        let height = u16::from_be_bytes([raw[7], raw[8]]);
        let palette_len = raw[9] as usize;
        let transparent = if raw[10] == 0xFF { None } else { Some(raw[10]) };
        let palette_start = 11;
        let palette_end = palette_start + palette_len * 3;
        if raw.len() < palette_end + 4 {
            return None;
        }
        let mut palette = Vec::with_capacity(palette_len);
        for index in 0..palette_len {
            let at = palette_start + index * 3;
            palette.push(Color::rgb(raw[at], raw[at + 1], raw[at + 2]));
        }
        let plane_len = u32::from_be_bytes([
            raw[palette_end],
            raw[palette_end + 1],
            raw[palette_end + 2],
            raw[palette_end + 3],
        ]) as usize;
        let plane_start = palette_end + 4;
        if raw.len() < plane_start + plane_len {
            return None;
        }
        let plane = &raw[plane_start..plane_start + plane_len];

        // The plane is run-length encoded: a count byte and the value it
        // repeats.
        let mut pixels = Vec::with_capacity(width as usize * height as usize);
        let mut at = 0;
        while at + 1 < plane.len() {
            let count = plane[at] as usize;
            let value = plane[at + 1];
            at += 2;
            for _ in 0..count {
                pixels.push(value);
            }
        }
        if pixels.len() != width as usize * height as usize {
            // A plane that does not fill the icon is a broken asset, not a
            // short icon.
            return None;
        }
        Some(Self {
            width,
            height,
            palette,
            pixels,
            transparent,
        })
    }

    /// The icon's width in pixels.
    pub const fn width(&self) -> u32 {
        self.width as u32
    }

    /// The icon's height in pixels.
    pub const fn height(&self) -> u32 {
        self.height as u32
    }

    /// The colour of the pixel at `(x, y)`, or `None` where the icon is
    /// transparent.
    pub fn color_at(&self, x: u32, y: u32) -> Option<Color> {
        if x >= self.width() || y >= self.height() {
            return None;
        }
        let index = self.pixels[y as usize * self.width as usize + x as usize];
        if Some(index) == self.transparent {
            return None;
        }
        self.palette.get(index as usize).copied()
    }

    /// Draw the icon into `surface` with its top-left corner at `(x, y)`.
    ///
    /// The transparent pixels are left alone, so the desktop's backdrop
    /// shows through where the artwork does not cover.
    pub fn draw(&self, surface: &mut Surface<'_>, x: i32, y: i32) {
        for row in 0..self.height() {
            for column in 0..self.width() {
                let Some(color) = self.color_at(column, row) else {
                    continue;
                };
                let px = x + column as i32;
                let py = y + row as i32;
                if px < 0 || py < 0 {
                    continue;
                }
                surface.put_pixel(px as u32, py as u32, color);
            }
        }
    }
}

/// The icon set, by name.
///
/// The names are the AROS ones: `RAM` is the RAM disk, `System` and
/// `Utilities` are the drawers of the same name, and `Shell` is the shell's
/// program icon.
pub mod assets {
    /// The RAM disk's icon.
    pub const RAM: &[u8] = include_bytes!("../assets/RAM.idat");
    /// The hard disk's icon.
    pub const DISK: &[u8] = include_bytes!("../assets/Disk.idat");
    /// The `System` drawer's icon.
    pub const SYSTEM: &[u8] = include_bytes!("../assets/System.idat");
    /// The `Utilities` drawer's icon.
    pub const UTILITIES: &[u8] = include_bytes!("../assets/Utilities.idat");
    /// The `Startup` drawer's icon.
    pub const STARTUP: &[u8] = include_bytes!("../assets/Startup.idat");
    /// The shell's program icon.
    pub const SHELL: &[u8] = include_bytes!("../assets/Shell.idat");
    /// The clock's program icon.
    pub const CLOCK: &[u8] = include_bytes!("../assets/Clock.idat");
    /// The `More` program's icon.
    pub const MORE: &[u8] = include_bytes!("../assets/More.idat");
    /// The hard-disk tool's icon.
    pub const HDTOOLBOX: &[u8] = include_bytes!("../assets/HDToolBox.idat");
    /// The icon editor's icon.
    pub const ICONEDIT: &[u8] = include_bytes!("../assets/IconEdit.idat");
    /// The calculator's icon.
    pub const CALCULATOR: &[u8] = include_bytes!("../assets/Calculator.idat");
    /// The commodities program's icon.
    pub const COMMODITIES: &[u8] = include_bytes!("../assets/Commodities.idat");
    /// The text editor's icon.
    pub const MEMACS: &[u8] = include_bytes!("../assets/Memacs.idat");
    /// The print-queue program's icon.
    pub const PRINTFILES: &[u8] = include_bytes!("../assets/PrintFiles.idat");
    /// The picture program's icon.
    pub const LACER: &[u8] = include_bytes!("../assets/Lacer.idat");
    /// The screen-dump program's icon.
    pub const SCREENDUMP: &[u8] = include_bytes!("../assets/Screendump.idat");
    /// the mail reader's icon.
    pub const WIMP: &[u8] = include_bytes!("../assets/WiMP.idat");
    /// the directory-opus program's icon.
    pub const DIRECTORYOPUS: &[u8] = include_bytes!("../assets/DirectoryOpus.idat");
}

#[cfg(test)]
extern crate std;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_asset_decodes() {
        // An asset that does not decode is a desktop that draws nothing, and
        // the failure would otherwise only show up on the screen.
        let all: [(&str, &[u8]); 18] = [
            ("RAM", assets::RAM),
            ("Disk", assets::DISK),
            ("System", assets::SYSTEM),
            ("Utilities", assets::UTILITIES),
            ("Startup", assets::STARTUP),
            ("Shell", assets::SHELL),
            ("Clock", assets::CLOCK),
            ("More", assets::MORE),
            ("HDToolBox", assets::HDTOOLBOX),
            ("IconEdit", assets::ICONEDIT),
            ("Calculator", assets::CALCULATOR),
            ("Commodities", assets::COMMODITIES),
            ("Memacs", assets::MEMACS),
            ("PrintFiles", assets::PRINTFILES),
            ("Lacer", assets::LACER),
            ("Screendump", assets::SCREENDUMP),
            ("WiMP", assets::WIMP),
            ("DirectoryOpus", assets::DIRECTORYOPUS),
        ];
        for (name, raw) in all {
            let icon = Icon::decode(raw).unwrap_or_else(|| panic!("{name} does not decode"));
            assert_eq!(icon.width(), 46, "{name} is the wrong width");
            assert_eq!(icon.height(), 46, "{name} is the wrong height");
            // Every AROS icon has a transparent background, and the corners
            // are where that shows.
            assert!(
                icon.color_at(0, 0).is_none(),
                "{name} has no transparent corner"
            );
        }
    }

    #[test]
    fn a_broken_asset_decodes_to_nothing() {
        assert!(Icon::decode(b"").is_none());
        assert!(Icon::decode(b"NOPE").is_none());
        assert!(Icon::decode(b"IDAT\x09\x00\x00").is_none());
    }
}
