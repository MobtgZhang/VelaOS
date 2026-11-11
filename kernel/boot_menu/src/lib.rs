//! Boot menu: terminal vs desktop selection at startup.
//!
//! # What this module is
//!
//! It draws a simple menu on both the framebuffer console — the display the
//! user is actually looking at — and the serial log, then waits for the user
//! to press a key or for the timeout to elapse. The default entry is
//! selected automatically after timeout_seconds, and the countdown is
//! redrawn on the screen each second so a waiting boot never looks hung.
//!
//! # Menu entries
//!   1. Boot to Safe Terminal (command-line only)
//!   2. Boot to Desktop (graphical Workbench)  [DEFAULT]
//!
//! # Returns
//!   Some(0) = terminal, Some(1) = desktop, None = timeout (use default).

#![no_std]

use core::sync::atomic::{AtomicU32, Ordering};

use vela_arch::{ps2, regs, serial};
use vela_kernel_console as console;

/// Boot menu selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BootChoice {
    Terminal,
    Desktop,
}

/// The boot menu state.
pub struct BootMenu {
    entries: &'static [(&'static str, BootChoice)],
    selected: AtomicU32,
    timeout_seconds: u32,
    default: u32,
}

static BOOT_MENU: spin::Once<BootMenu> = spin::Once::new();

impl BootMenu {
    pub fn new(
        entries: &'static [(&'static str, BootChoice)],
        timeout_seconds: u32,
        default: u32,
    ) -> Self {
        Self {
            entries,
            selected: AtomicU32::new(0),
            timeout_seconds,
            default,
        }
    }

    /// Run the interactive menu. Returns the user's choice or None on timeout.
    pub fn run(&self) -> Option<BootChoice> {
        // Draw the menu on serial and on the framebuffer console.
        crate::draw_menu(self);

        // Wait for keypress or timeout.
        let ticks_per_second = vela_arch::apic::ticks_per_second();
        let deadline = vela_arch::apic::ticks() + (self.timeout_seconds as u64 * ticks_per_second);

        // The countdown is redrawn when the second it displays changes, so a
        // waiting boot visibly counts down rather than sitting still.
        let mut shown_seconds = self.timeout_seconds as u64;

        loop {
            // Check for a keypress from either the PS/2 keyboard or COM1.
            if let Some(byte) = crate::poll_key() {
                match byte {
                    b'1' => {
                        self.selected.store(0, Ordering::Release);
                        return Some(BootChoice::Terminal);
                    }
                    b'2' => {
                        self.selected.store(1, Ordering::Release);
                        return Some(BootChoice::Desktop);
                    }
                    b'\r' | b'\n' => {
                        let sel = self.selected.load(Ordering::Acquire);
                        return Some(self.entries[sel as usize].1);
                    }
                    0x1B => {
                        // Escape: select the highlighted default (entry 0 = terminal).
                        self.selected.store(0, Ordering::Release);
                        return Some(BootChoice::Terminal);
                    }
                    _ => {}
                }
            }

            // The countdown: one rewrite per second, in place. The console's
            // `\r` returns to the line's start, and the fixed-width payload
            // pads over the wider numbers that came before.
            let remaining =
                deadline.saturating_sub(vela_arch::apic::ticks()) / ticks_per_second.max(1);
            if remaining != shown_seconds {
                shown_seconds = remaining;
                console::print("\rSelect an entry (1-2). Desktop starts in ");
                for byte in countdown_text(remaining) {
                    console::putc(byte);
                }
            }

            // Check timeout.
            if vela_arch::apic::ticks() >= deadline {
                return None; // timeout - use default
            }

            regs::pause();
        }
    }

    /// Get the selected entry index.
    pub fn selected(&self) -> u32 {
        self.selected.load(Ordering::Acquire)
    }

    /// Get the default entry index.
    pub fn default(&self) -> u32 {
        self.default
    }

    /// The choice at an entry index.
    pub fn choice(&self, index: u32) -> Option<BootChoice> {
        self.entries.get(index as usize).map(|(_, choice)| *choice)
    }
}

/// Get the global boot menu.
pub fn boot_menu() -> &'static BootMenu {
    BOOT_MENU
        .get()
        .expect("boot menu not installed; kernel/entry must call init_boot_menu")
}

/// Install the boot menu.
///
/// # Safety
///
/// Must be called once during boot, before the menu is run. `call_once` makes
/// repeated calls harmless, but the entries are a static table and the menu is
/// a process-wide singleton.
pub unsafe fn init_boot_menu() {
    let entries: &'static [(&'static str, BootChoice)] = &[
        ("1. Boot to Safe Terminal", BootChoice::Terminal),
        ("2. Boot to Desktop  [DEFAULT]", BootChoice::Desktop),
    ];
    BOOT_MENU.call_once(|| BootMenu::new(entries, 10, 1));
}

/// Draw the menu on the serial console.
fn draw_menu(menu: &BootMenu) {
    serial::puts("\n");
    serial::puts("========================================\n");
    serial::puts("   VelaOS Boot Menu\n");
    serial::puts("========================================\n");
    for (i, (text, _)) in menu.entries.iter().enumerate() {
        if i as u32 == menu.selected() {
            serial::puts("   >> ");
        } else {
            serial::puts("      ");
        }
        serial::puts(text);
        serial::puts("\n");
    }
    serial::puts("\n");
    serial::puts("Select an entry (1-2) or wait ");
    serial::puts_dec(menu.timeout_seconds as u64);
    serial::puts(" seconds for default...\n");

    // The same menu on the framebuffer console, because the serial log is
    // a file in the ordinary QEMU run and the display is what the user is
    // actually looking at. The console scrolls, so printing after the boot
    // log puts the menu just under it, in view.
    console::print("\n");
    console::print("========================================\n");
    console::print("   VelaOS Boot Menu\n");
    console::print("========================================\n");
    for (i, (text, _)) in menu.entries.iter().enumerate() {
        if i as u32 == menu.selected() {
            console::print("   >> ");
        } else {
            console::print("      ");
        }
        console::print(text);
        console::print("\n");
    }
    console::print("\n");
    // The countdown line is one cell row, rewritten in place each second by
    // [`run`]: `\r` returns to the line's start, and the trailing spaces pad
    // over whatever a longer number left behind. The line is deliberately
    // *not* terminated: the rewrite only works while the cursor is still on
    // it, and nothing else prints to the console while the menu runs.
    console::print("Select an entry (1-2). Desktop starts in ");
    for byte in countdown_text(menu.timeout_seconds as u64) {
        console::putc(byte);
    }
}

/// The countdown line's payload, padded to a fixed width.
///
/// A fixed width is what makes the in-place rewrite work: "10" is one cell
/// wider than "9", and a rewrite without padding would leave a stray digit.
/// Returns the digits left-justified in a fixed 12-byte buffer of spaces.
fn countdown_text(seconds: u64) -> [u8; 12] {
    // Decimal, left-justified in 12 bytes of spaces. The buffer is one byte
    // longer than the widest value `u32` can hold, so no number can overflow
    // it, and the write below never runs off the end.
    let mut text = [b' '; 12];
    let mut digits = [0u8; 20];
    let mut len = 0;
    let mut value = seconds;
    if value == 0 {
        digits[0] = b'0';
        len = 1;
    }
    while value > 0 {
        digits[len] = b'0' + (value % 10) as u8;
        len += 1;
        value /= 10;
    }
    // The digits came out backwards; lay them down left-justified.
    for (i, digit) in digits[..len].iter().rev().enumerate() {
        text[i] = *digit;
    }
    text
}

/// Poll the PS/2 keyboard and COM1 for one character (non-blocking).
///
/// The boot menu runs before `init`, and QEMU's serial port is configured as
/// a file, so serial input is normally absent. The keyboard is the input the
/// user actually has: a scancode set 1 key is translated here without pulling
/// in the full `vela-kernel-input` service, because the menu only needs the
/// digits, Return and Escape.
fn poll_key() -> Option<u8> {
    // SAFETY: the PS/2 status port is always readable; the data port is read
    // only when the output-full bit says a byte is waiting.
    unsafe {
        let status = ps2::read_status();
        if status & 0x01 != 0 {
            let scancode = ps2::read_data();
            // Released bit is the high bit; ignore releases.
            let code = scancode & 0x7F;
            let ch = match code {
                0x02 => Some(b'1'),
                0x03 => Some(b'2'),
                0x1C => Some(b'\r'),
                0x01 => Some(0x1B),
                _ => None,
            };
            if scancode & 0x80 == 0 {
                if let Some(ch) = ch {
                    return Some(ch);
                }
            }
        }
    }

    // COM1 may still carry a character when the machine is run with a serial
    // terminal rather than a file-backed console.
    serial::getc()
        .map(|byte| match byte {
            b'1' => b'1',
            b'2' => b'2',
            b'\r' => b'\r',
            b'\n' => b'\n',
            _ => 0,
        })
        .filter(|byte| *byte != 0)
}
