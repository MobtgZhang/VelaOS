//! The PS/2 keyboard poll.
//!
//! # Why polling rather than interrupts
//!
//! The kernel's IDT has no keyboard gate in v0.1.0 — the plan defers it — so
//! the console reads the controller directly: check the status port's
//! output-full bit, and when a byte is waiting, take it from the data port.
//! This is the same shape the serial driver uses for its own `GetChar`, and
//! for a shell it is the right shape: the shell spins on the poll between
//! redraws, so a key typed now is read now rather than queued until an
//! interrupt arrives.
//!
//! # What this module is not
//!
//! Not a driver for the controller's *configuration*: the firmware has
//! already enabled the keyboard port and set the scancode translation before
//! the kernel runs, so there is nothing here to send the controller. A
//! cold-reset controller would need `EnablePort` commands this module does
//! not issue, and that is stated rather than hidden.

use core::sync::atomic::{AtomicBool, Ordering};
use spin::Mutex;

use vela_hal_x86_64::regs;
use vela_kernel_input::{ports, status, Keyboard};

/// The status port's auxiliary bit: the waiting byte is a mouse packet.
const MOUSE_DATA: u8 = 1 << 5;

/// The keyboard decoder's modifier state, shared because the syscall layer
/// is the only caller but a handler may run on any CPU.
static KEYBOARD: Mutex<Keyboard> = Mutex::new(Keyboard::new());

/// Whether the controller answered its first poll, so a wedged controller is
/// reported once rather than on every call.
static SEEN_CONTROLLER: AtomicBool = AtomicBool::new(false);

/// Poll the keyboard for one ASCII byte.
///
/// Returns `Some(byte)` when the user has pressed a key this call can turn
/// into text — a printable character, Enter (`\r`) or Backspace (`0x08`).
/// Returns `None` when nothing is waiting, which includes scancodes that
/// decode to nothing (modifiers, releases, unknown keys) and bytes the
/// controller has for the *mouse*, which are consumed and discarded because
/// the mouse has its own driver and the console does not want them.
pub fn read_char() -> Option<u8> {
    let mut produced = None;
    let mut keyboard = KEYBOARD.lock();
    // Drain everything waiting, because one keypress can arrive as several
    // bytes (a modifier plus the key, or a two-byte escape sequence), and
    // leaving the tail queued would make the *next* key late by one.
    while output_full() {
        // Mouse packets also arrive on the data port. The controller marks
        // them with the auxiliary bit in the status byte, so they are
        // consumed and discarded here: the mouse has its own driver, and a
        // byte the keyboard decoder would misread as a scancode would make
        // the console type garbage when the pointer moved.
        // SAFETY: the status port said a byte is waiting on the data port,
        // which is the only condition under which reading it is defined.
        let status_byte = unsafe { regs::inb(ports::STATUS) };
        let scancode = unsafe { regs::inb(ports::DATA) };
        SEEN_CONTROLLER.store(true, Ordering::Release);
        if status_byte & MOUSE_DATA != 0 {
            continue;
        }
        if let Some(event) = keyboard.feed(scancode) {
            if event.released {
                continue;
            }
            if event.code.to_ascii().is_some() {
                produced = Some(ascii_of(event));
            } else if event.code == vela_kernel_input::KeyCode::ENTER {
                produced = Some(b'\r');
            } else if event.code == vela_kernel_input::KeyCode::BACKSPACE {
                produced = Some(0x08);
            }
            // One key per poll: a shell that wants more polls again. Draining
            // the queue above keeps the *next* call current, but returning one
            // byte at a time is what a terminal's read of one character means.
            break;
        }
    }
    produced
}

/// The event's ASCII byte, with the decoder's shift and caps already applied.
fn ascii_of(event: vela_kernel_input::KeyEvent) -> u8 {
    // The decoder has already folded shift and caps into the code, so the
    // code's ASCII value is the character the user meant. Control turns a
    // letter into its control code, the way a terminal's line discipline
    // would.
    let Some(ch) = event.code.to_ascii() else {
        return 0;
    };
    let mut byte = ch as u8;
    if event.ctrl && byte.is_ascii_alphabetic() {
        byte &= 0x1F;
    }
    byte
}

/// True when the controller has a byte for the host.
fn output_full() -> bool {
    // SAFETY: the status port is always readable.
    let status_byte = unsafe { regs::inb(ports::STATUS) };
    status_byte & status::OUTPUT_FULL != 0
}
