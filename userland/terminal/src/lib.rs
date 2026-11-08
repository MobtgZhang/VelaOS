//! Terminal emulator: a VT100-style terminal inside an Intuition window.
//!
//! # What this crate is
//!
//! A userland application. It parses the character stream it is given, keeps a
//! scrollback, and renders the visible rows through `graphics.library` into a
//! window the caller opened. It reaches the kernel only through the SDK:
//! no kernel type, no kernel pointer and no architecture-specific code.
//!
//! # Rendering is not framebuffer writing
//!
//! A user program cannot write the display's pixels directly — the framebuffer
//! is kernel-owned and the syscall boundary deliberately has no raster pointer.
//! So `render` asks the graphics service to fill the window's own raster and to
//! draw the text; the kernel owns the pixel storage and composites the result.

#![no_std]

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;

use vela_sys::abi::Rectangle;
use vela_sys::api::{self, Pen, Window};
use vela_sys::Result;

/// The character cell's height in the kernel's text font.
pub const CELL_H: i16 = 16;

/// The terminal emulator state.
pub struct Terminal {
    rows: u16,
    cols: u16,
    cursor_x: u16,
    cursor_y: u16,
    scrollback: Vec<String>,
    current_line: String,
    window: Window,
    bg_pen: Pen,
    fg_pen: Pen,
}

impl Terminal {
    /// Create a terminal over an existing window.
    ///
    /// The two pens are allocated from the default palette; the background is
    /// black and the foreground is white, which is the terminal's own choice
    /// rather than the desktop theme's.
    pub fn new(window: Window, rows: u16, cols: u16) -> Self {
        let bg = api::graphics::alloc_pen(0, 0x0000_0000);
        let fg = api::graphics::alloc_pen(0, 0x00FF_FFFF);
        Self {
            rows,
            cols,
            cursor_x: 0,
            cursor_y: 0,
            scrollback: Vec::new(),
            current_line: String::new(),
            window,
            bg_pen: bg,
            fg_pen: fg,
        }
    }

    /// Print one character.
    pub fn putchar(&mut self, ch: char) {
        match ch {
            '\n' => {
                self.scrollback.push(self.current_line.clone());
                self.current_line.clear();
                self.cursor_x = 0;
                self.cursor_y = self.cursor_y.saturating_add(1);
            }
            '\r' => {
                self.cursor_x = 0;
            }
            '\x08' => {
                if self.cursor_x > 0 {
                    self.cursor_x -= 1;
                    self.current_line.pop();
                }
            }
            ch => {
                self.current_line.push(ch);
                self.cursor_x = self.cursor_x.saturating_add(1);
                if self.cursor_x >= self.cols {
                    self.scrollback.push(self.current_line.clone());
                    self.current_line.clear();
                    self.cursor_x = 0;
                    self.cursor_y = self.cursor_y.saturating_add(1);
                }
            }
        }
    }

    /// Print a string.
    pub fn print(&mut self, s: &str) {
        for ch in s.chars() {
            self.putchar(ch);
        }
    }

    /// Print a line followed by a newline.
    pub fn println(&mut self, s: &str) {
        self.print(s);
        self.putchar('\n');
    }

    /// Clear the terminal state.
    pub fn clear(&mut self) {
        self.scrollback.clear();
        self.current_line.clear();
        self.cursor_x = 0;
        self.cursor_y = 0;
    }

    /// The current input line.
    pub fn line(&self) -> &str {
        &self.current_line
    }

    /// Finish the current line and return it.
    pub fn submit_line(&mut self) -> String {
        let line = self.current_line.clone();
        self.current_line.clear();
        self.cursor_x = 0;
        self.putchar('\n');
        line
    }

    /// Render the terminal into the window's raster.
    ///
    /// The background fill is one `FillRect`; each visible row is one `Text`
    /// call. The kernel measures the string and draws it with its own font, so
    /// the terminal does not need a glyph table of its own.
    pub fn render(&self) -> Result<()> {
        let (width, height) = api::graphics::raster_info(self.window)?;
        let full = Rectangle::new(0, 0, width as u16, height as u16);
        api::graphics::fill_rect(self.window, self.bg_pen, &full)?;

        // Show at most `rows` lines ending at the current line.
        let total = self.scrollback.len() + 1;
        let first = total.saturating_sub(self.rows as usize);
        let mut y: i16 = 0;
        for line in self.scrollback.iter().skip(first) {
            if y as u32 + CELL_H as u32 > height {
                break;
            }
            api::graphics::text(self.window, self.fg_pen, 0, y, line)?;
            y += CELL_H;
        }
        // The current line is always drawn at the last row.
        if y as u32 + CELL_H as u32 <= height {
            api::graphics::text(self.window, self.fg_pen, 0, y, &self.current_line)?;
        }

        // A simple block cursor at the current cell, if it is on-screen.
        if self.cursor_y < self.rows {
            let cx = (self.cursor_x.min(self.cols.saturating_sub(1)) as i16) * 8;
            let cy = (self.cursor_y as i16) * CELL_H;
            if cx >= 0 && cy >= 0 && (cy as u32) < height {
                api::graphics::write_pixel(self.window, self.fg_pen, cx, cy)?;
            }
        }
        Ok(())
    }

    /// The Window this terminal draws into.
    pub fn window(&self) -> Window {
        self.window
    }

    /// Release the terminal's pens.
    pub fn close(&self) {
        let _ = api::graphics::free_pen(0, self.bg_pen);
        let _ = api::graphics::free_pen(0, self.fg_pen);
    }
}
