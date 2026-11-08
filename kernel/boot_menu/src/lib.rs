//! Boot menu: terminal vs desktop selection at startup.
//!
//! # What this module is
//!
//! It draws a simple text menu on the framebuffer/serial console and waits
//! for the user to press a key or for the timeout to elapse. The default
//! entry is highlighted and selected automatically after timeout_seconds.
//!
//! # Menu entries
//!   1. Boot to Terminal (command-line only)
//!   2. Boot to Desktop (graphical Workbench)  [DEFAULT]
//!
//! # Returns
//!   Some(0) = terminal, Some(1) = desktop, None = timeout (use default).

#![no_std]

use core::sync::atomic::{AtomicU32, Ordering};

use vela_arch::{ps2, regs, serial};

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
        // Draw the menu on serial.
        crate::draw_menu(self);

        // Wait for keypress or timeout.
        let ticks_per_second = vela_arch::apic::ticks_per_second();
        let deadline = vela_arch::apic::ticks() + (self.timeout_seconds as u64 * ticks_per_second);

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
                    _ => {}
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
        ("1. Boot to Terminal", BootChoice::Terminal),
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
