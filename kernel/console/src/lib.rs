//! The kernel text console.
//!
//! # What this crate is
//!
//! Two things a boot-time terminal needs, in one place:
//!
//! * [`console`] — a character grid rendered onto the framebuffer with the
//!   embedded 8×16 bitmap font. This is what makes `PutChar` visible in the
//!   QEMU window: without it, a guest's output goes to the serial log only,
//!   and the screen the user is looking at stays silent.
//! * [`keyboard`] — a poll of the PS/2 controller that turns scancodes into
//!   ASCII bytes. This is what makes `GetChar` able to read the keys the
//!   user types into the QEMU window.
//!
//! # Why both live here rather than in their drivers
//!
//! The framebuffer belongs to the display and the controller to the input
//! driver, but a *console* is the thing that joins them: text goes in,
//! keystrokes come out. Neither driver alone is a terminal, and the syscall
//! layer should not have to know glyph geometry or scancode tables.

#![no_std]

extern crate alloc;

use core::sync::atomic::{AtomicBool, Ordering};
use spin::Mutex;

pub mod keyboard;

/// Text colour: warm light grey, readable on dark.
const FG: u32 = 0x00E8_E8DC;
/// Background colour: near-black with a blue cast, the classic terminal look.
const BG: u32 = 0x0012_1218;

/// Whether [`console::init`] has run with a real display.
static HAVE_DISPLAY: AtomicBool = AtomicBool::new(false);

/// The console's state, guarded because a syscall handler may run while
/// another handler (or the boot code) touches the same grid.
static STATE: Mutex<Option<Console>> = Mutex::new(None);

/// One text console over a framebuffer.
struct Console {
    /// The framebuffer's base address, which is also its virtual address:
    /// the kernel maps the framebuffer identity.
    base: u64,
    /// Screen width in pixels.
    width: u32,
    /// Screen height in pixels.
    height: u32,
    /// Row stride in bytes.
    stride: u32,
    /// The cursor's column, in cells.
    col: u32,
    /// The cursor's row, in cells.
    row: u32,
}

impl Console {
    /// The grid's width in cells.
    fn cols(&self) -> u32 {
        self.width / vela_text::GLYPH_WIDTH
    }

    /// The grid's height in cells.
    fn rows(&self) -> u32 {
        self.height / vela_text::GLYPH_HEIGHT
    }

    /// Write one byte to the pixel at (`x`, `y`).
    ///
    /// # Safety
    ///
    /// `x` must be under `width` and `y` under `height`, which the callers
    /// compute from the grid bounds.
    unsafe fn put_pixel(&mut self, x: u32, y: u32, color: u32) {
        let addr = self.base + (y as u64) * (self.stride as u64) + (x as u64) * 4;
        // SAFETY: the framebuffer is mapped for the kernel's life, and the
        // bounds check is the caller's job, done just above.
        unsafe { (addr as *mut u32).write_volatile(color) };
    }

    /// Draw one glyph cell at cell (`cx`, `cy`).
    fn draw_cell(&mut self, cx: u32, cy: u32, ch: char) {
        let glyph = vela_text::glyph(ch);
        let px = cx * vela_text::GLYPH_WIDTH;
        let py = cy * vela_text::GLYPH_HEIGHT;
        for (row, bits) in glyph.iter().enumerate() {
            for col in 0..vela_text::GLYPH_WIDTH as u8 {
                let on = bits & (1 << (vela_text::GLYPH_WIDTH as u8 - 1 - col)) != 0;
                let color = if on { FG } else { BG };
                // SAFETY: px + col and py + row are inside the cell, and the
                // cell is inside the grid because the cursor was clamped.
                unsafe { self.put_pixel(px + col as u32, py + row as u32, color) };
            }
        }
    }

    /// Clear one cell to the background colour.
    fn erase_cell(&mut self, cx: u32, cy: u32) {
        self.draw_cell(cx, cy, ' ');
    }

    /// Shift every row up by one cell row, then clear the last row.
    fn scroll(&mut self) {
        let rows = self.rows();
        let bytes_per_row = self.stride as u64;
        for r in 1..rows {
            let from = self.base + (r as u64) * bytes_per_row;
            let to = self.base + ((r - 1) as u64) * bytes_per_row;
            // SAFETY: both addresses are inside the framebuffer, the copy is
            // row-sized, and rows do not overlap because `to` is one row
            // above `from`.
            unsafe { core::ptr::copy(from as *const u8, to as *mut u8, bytes_per_row as usize) };
        }
        // Clear the last row.
        for cx in 0..self.cols() {
            self.erase_cell(cx, rows - 1);
        }
    }

    /// Advance the cursor past a printed character, scrolling at the bottom.
    fn advance(&mut self) {
        self.col += 1;
        if self.col >= self.cols() {
            self.col = 0;
            self.newline();
        }
    }

    /// Move to the start of the next row, scrolling when that would fall off
    /// the bottom.
    fn newline(&mut self) {
        self.row += 1;
        if self.row >= self.rows() {
            self.row = self.rows() - 1;
            self.scroll();
        }
    }
}

/// Make the console render onto a framebuffer.
///
/// Called once from kernel entry with the boot handoff's geometry. A call
/// with a zero address means there is no display; the console then stays a
/// no-op and every output path degrades to serial-only, which is the
/// headless configuration rather than an error.
///
/// # Safety
///
/// The address must be a framebuffer mapped for the kernel's life, and the
/// geometry must describe it. Both come from the boot handoff, which the
/// loader validated against the GOP.
pub unsafe fn init(address: u64, width: u32, height: u32, stride_bytes: u32) {
    if address == 0 || width == 0 || height == 0 {
        return;
    }
    let stride = if stride_bytes == 0 {
        width * 4
    } else {
        stride_bytes
    };
    // Building the console is safe: the *writes* it will later make are the
    // caller's responsibility, and they only happen once `HAVE_DISPLAY` says
    // the address was installed.
    let mut console = Console {
        base: address,
        width,
        height,
        stride,
        col: 0,
        row: 0,
    };
    console.clear();
    STATE.lock().replace(console);
    HAVE_DISPLAY.store(true, Ordering::Release);
}

impl Console {
    /// Fill the whole grid with the background colour and home the cursor.
    fn clear(&mut self) {
        for cy in 0..self.rows() {
            for cx in 0..self.cols() {
                self.erase_cell(cx, cy);
            }
        }
        self.col = 0;
        self.row = 0;
    }
}

/// Write one byte to the console.
///
/// The three control characters a terminal needs are handled here rather
/// than in every caller: `\n` (newline), `\r` (carriage return) and `\x08`
/// (backspace). Every other byte is printed as its glyph, or as `?` when the
/// font has none.
pub fn putc(byte: u8) {
    if !HAVE_DISPLAY.load(Ordering::Acquire) {
        return;
    }
    let mut state = STATE.lock();
    let Some(console) = state.as_mut() else {
        return;
    };
    match byte {
        // A newline is a full line break: the next output starts at the
        // left edge, the way a terminal's line feed does. The column reset
        // lives here rather than only in `\r` because a log line that ends
        // in `\n` alone (serial's convention) must still start over.
        b'\n' => {
            console.col = 0;
            console.newline();
        }
        b'\r' => console.col = 0,
        0x08 => {
            // Backspace moves the cursor back over a non-empty row and
            // erases the cell it was on, which is what a terminal's rubout
            // looks like.
            if console.col > 0 {
                console.col -= 1;
                let (c, r) = (console.col, console.row);
                console.erase_cell(c, r);
            }
        }
        _ => {
            let ch = byte as char;
            let (c, r) = (console.col, console.row);
            console.draw_cell(c, r, ch);
            console.advance();
        }
    }
}

/// Write a string, byte by byte.
pub fn print(s: &str) {
    for byte in s.bytes() {
        putc(byte);
    }
}

/// True when there is a display to draw on.
pub fn has_display() -> bool {
    HAVE_DISPLAY.load(Ordering::Acquire)
}

#[cfg(test)]
extern crate std;
