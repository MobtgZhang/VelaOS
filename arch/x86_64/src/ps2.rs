//! PS/2 controller port access.
//!
//! The PS/2 controller's command/status/data ports are x86-specific I/O
//! ports, so the *port access* lives here in the architecture crate. The
//! *decoding* of the byte stream into scancodes and mouse packets remains in
//! `vela-kernel-input`, which is architecture-neutral and unit-testable.

use crate::regs;

/// The PS/2 data port.
pub const DATA_PORT: u16 = 0x60;
/// The PS/2 status port.
pub const STATUS_PORT: u16 = 0x64;
/// The PS/2 command port (same address as the status port on most chips).
pub const COMMAND_PORT: u16 = 0x64;

/// Read the controller's status byte.
///
/// # Safety
///
/// The status port is always readable; this is a plain port read with no
/// side effects.
#[inline]
pub unsafe fn read_status() -> u8 {
    // SAFETY: reading the PS/2 status port is always defined.
    unsafe { regs::inb(STATUS_PORT) }
}

/// Read a byte from the controller's data port.
///
/// Only call this when `read_status` reports the output-full bit, or the
/// byte read is meaningless.
///
/// # Safety
///
/// The caller must ensure a byte is actually waiting on the data port.
#[inline]
pub unsafe fn read_data() -> u8 {
    // SAFETY: the caller guarantees a byte is waiting.
    unsafe { regs::inb(DATA_PORT) }
}

/// Write a byte to the controller's command port.
///
/// # Safety
///
/// Writing the command port can reconfigure the controller; callers must
/// know what they are doing.
#[inline]
pub unsafe fn write_command(byte: u8) {
    // SAFETY: writing the PS/2 command port is defined for any byte.
    unsafe { regs::outb(COMMAND_PORT, byte) }
}
