//! Input-device plumbing, named after the x86-64 `ps2` for surface parity.
//!
//! # The honest version: there is no PS/2 on QEMU `virt`
//!
//! The PS/2 controller (8042) and its `0x60`/`0x64` port pair are PC-legacy
//! hardware that a RISC-V platform simply does not have. QEMU's `virt`
//! machine wires its keyboard straight into the [`crate::serial`] UART:
//! bytes typed into the window appear as UART receive bytes, and the same
//! driver that prints the kernel's logs is the only input device there is.
//!
//! This module therefore routes the x86-64 surface's *role* — "get me the
//! next input byte, or say there is none" — onto the UART, and documents
//! the mapping in one place. The *decoding* of the byte stream into
//! scancodes and escape sequences stays in `vela-kernel-input`, which is
//! architecture-neutral and unit-tested, exactly as the x86-64 module's
//! documentation promises.
//!
//! # What a future board changes
//!
//! A real RISC-V board with a USB or PS/2 controller replaces [`has_input`]
//! and [`read_data`] with that device's access; the input stack consumes
//! only these two functions, so the change is local to this module.

use crate::serial;

/// True when an input byte is waiting.
///
/// On QEMU `virt` this polls the UART's line-status register; there is no
/// separate controller to interrogate.
pub fn has_input() -> bool {
    serial::try_getb().is_some()
}

/// Pop one input byte, or `None` when nothing is waiting.
///
/// The caller polls this in a loop between redraws — the same contract the
/// x86-64 `read_data` offers the input stack, with the UART doing the
/// waiting instead of a controller FIFO.
pub fn read_data() -> Option<u8> {
    serial::try_getb()
}

#[cfg(test)]
extern crate std;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fresh_uart_has_no_input() {
        // The host UART reports "no byte" (line status reads zero off bare
        // metal), so the input surface starts empty and never blocks.
        assert!(!has_input());
        assert_eq!(read_data(), None);
    }

    #[test]
    fn the_surface_is_poll_shaped() {
        // Both functions return promptly by construction: they wrap the
        // UART's non-blocking `try_getb`, and nothing here spins.
        has_input();
        read_data();
    }
}
