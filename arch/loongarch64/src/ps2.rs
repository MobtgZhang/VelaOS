//! Input plumbing.
//!
//! # There is no PS/2 controller here
//!
//! The x86-64 crate talks to a PS/2 controller because the PC has one.
//! A LoongArch board does not: its keyboard is on the system chip's
//! keyboard controller, reached through the chip's own registers, or
//! through a USB host controller.
//!
//! The surface the facade imports is kept, and it is honest: the input
//! path is empty until the machine's own controller is driven. A program
//! that asks for a byte gets `None`, which is the same answer the x86-64
//! driver gives when no key has been pressed.
//!
//! The plan for the input device is in `reports/11-input.md`; this crate
//! is only the place where the board-specific part will live.

/// Read one byte of input, if a byte is waiting.
pub fn get_byte() -> Option<u8> {
    None
}

/// True when a byte of input is waiting.
pub fn input_ready() -> bool {
    false
}

#[cfg(test)]
extern crate std;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_input_path_is_empty_until_the_controller_is_driven() {
        assert_eq!(get_byte(), None);
        assert!(!input_ready());
    }
}
