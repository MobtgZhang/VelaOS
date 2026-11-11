//! HID boot-protocol report decoders and set-1 keyboard translation.

use vela_kernel_input::MouseEvent;

/// Translate a keyboard-page HID usage to a non-extended PS/2 set-1 byte.
pub(crate) const fn usage_to_set1(usage: u8) -> Option<u8> {
    let letters = [
        0x1E, 0x30, 0x2E, 0x20, 0x12, 0x21, 0x22, 0x23, 0x17, 0x24, 0x25, 0x26, 0x32,
        0x31, 0x18, 0x19, 0x10, 0x13, 0x1F, 0x14, 0x16, 0x2F, 0x11, 0x2D, 0x15, 0x2C,
    ];
    Some(match usage {
        0x04..=0x1d => letters[(usage - 0x04) as usize],
        0x1e..=0x27 => usage - 0x1e + 0x02,
        0x28 => 0x1c, // Enter
        0x29 => 0x01, // Escape
        0x2a => 0x0e, // Backspace
        0x2b => 0x0f, // Tab
        0x2c => 0x39, // Space
        0x2d => 0x0c,
        0x2e => 0x0d,
        0x2f => 0x1a,
        0x30 => 0x1b,
        0x31 => 0x2b,
        0x33 => 0x27,
        0x34 => 0x28,
        0x35 => 0x29,
        0x36 => 0x33,
        0x37 => 0x34,
        0x38 => 0x35,
        0x39 => 0x3a,
        0x3a..=0x43 => usage - 0x3a + 0x3b,
        0x44 => 0x57,
        0x45 => 0x58,
        0x4a => 0x47, // Home
        0x4b => 0x49, // Page Up
        0x4c => 0x53, // Delete
        0x4d => 0x4f, // End
        0x4e => 0x51, // Page Down
        0x4f => 0x4d, // Right
        0x50 => 0x4b, // Left
        0x51 => 0x50, // Down
        0x52 => 0x48, // Up
        _ => return None,
    })
}

const MODIFIER_USAGES: [(u8, u8); 6] = [
    (0, 0x1d), // left control
    (1, 0x2a), // left shift
    (2, 0x38), // left alt
    (4, 0x1d), // right control (the console also accepts its set-1 partner)
    (5, 0x36), // right shift
    (6, 0x38), // right alt
];

/// A fixed-capacity scancode batch; producing events never allocates.
pub(crate) struct Scancodes {
    bytes: [u8; 24],
    len: usize,
}

impl Scancodes {
    const fn new() -> Self {
        Self {
            bytes: [0; 24],
            len: 0,
        }
    }

    fn push(&mut self, byte: u8) {
        if self.len < self.bytes.len() {
            self.bytes[self.len] = byte;
            self.len += 1;
        }
    }

    pub(crate) fn as_slice(&self) -> &[u8] {
        &self.bytes[..self.len]
    }
}

/// Previous state for one boot keyboard's modifier byte and six key array.
pub(crate) struct KeyboardState {
    previous: [u8; 8],
}

impl KeyboardState {
    pub(crate) const fn new() -> Self {
        Self { previous: [0; 8] }
    }

    /// Diff a report, returning presses and releases in modifier-first order.
    /// HID error/rollover reports leave the previous state untouched so they
    /// cannot manufacture a burst of phantom releases.
    pub(crate) fn feed(&mut self, report: &[u8]) -> Scancodes {
        let mut output = Scancodes::new();
        if report.len() < 8 {
            return output;
        }
        let mut current = [0u8; 8];
        current.copy_from_slice(&report[..8]);
        if current[2..].iter().any(|key| matches!(*key, 1..=3)) {
            return output;
        }

        for (bit, scancode) in MODIFIER_USAGES {
            let was = self.previous[0] & (1 << bit) != 0;
            let is = current[0] & (1 << bit) != 0;
            if was != is {
                output.push(if is { scancode } else { scancode | 0x80 });
            }
        }

        // Release keys which disappeared before adding newly pressed keys.
        for old in self.previous[2..].iter().copied() {
            if old != 0 && !current[2..].contains(&old) {
                if let Some(scancode) = usage_to_set1(old) {
                    output.push(scancode | 0x80);
                }
            }
        }
        for new in current[2..].iter().copied() {
            if new != 0 && !self.previous[2..].contains(&new) {
                if let Some(scancode) = usage_to_set1(new) {
                    output.push(scancode);
                }
            }
        }
        self.previous = current;
        output
    }
}

/// Absolute pointer state belonging to one relative HID boot mouse.
pub(crate) struct MouseState {
    x: i32,
    y: i32,
    buttons: u8,
}

impl MouseState {
    pub(crate) const fn new() -> Self {
        Self {
            x: 0,
            y: 0,
            buttons: 0,
        }
    }

    /// Consume the common three-byte boot report. A fourth wheel byte is
    /// accepted but ignored because VelaOS's MouseEvent has no wheel field.
    pub(crate) fn feed(&mut self, report: &[u8], width: u32, height: u32) -> Option<MouseEvent> {
        if report.len() < 3 {
            return None;
        }
        let buttons = report[0] & 7;
        let changed = buttons ^ self.buttons;
        let dx = report[1] as i8 as i32;
        let dy = report[2] as i8 as i32;
        self.buttons = buttons;
        self.x = clamp_axis(self.x.saturating_add(dx), width);
        self.y = clamp_axis(self.y.saturating_add(dy), height);
        if dx == 0 && dy == 0 && changed == 0 {
            return None;
        }
        Some(MouseEvent {
            x: self.x,
            y: self.y,
            buttons,
            changed,
        })
    }
}

fn clamp_axis(value: i32, bound: u32) -> i32 {
    if bound == 0 {
        return value;
    }
    let maximum = bound.saturating_sub(1).min(i32::MAX as u32) as i32;
    value.clamp(0, maximum)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn letters_digits_and_function_keys_use_set1() {
        assert_eq!(usage_to_set1(0x04), Some(0x1e));
        assert_eq!(usage_to_set1(0x05), Some(0x30));
        assert_eq!(usage_to_set1(0x1d), Some(0x2c));
        assert_eq!(usage_to_set1(0x1e), Some(0x02));
        assert_eq!(usage_to_set1(0x27), Some(0x0b));
        assert_eq!(usage_to_set1(0x28), Some(0x1c));
        assert_eq!(usage_to_set1(0x3a), Some(0x3b));
        assert_eq!(usage_to_set1(0x43), Some(0x44));
        assert_eq!(usage_to_set1(0x44), Some(0x57));
        assert_eq!(usage_to_set1(0x45), Some(0x58));
    }

    #[test]
    fn keyboard_diff_emits_modifier_before_a_press_and_release() {
        let mut keyboard = KeyboardState::new();
        let down = keyboard.feed(&[2, 0, 4, 0, 0, 0, 0, 0]);
        assert_eq!(down.as_slice(), &[0x2a, 0x1e]);
        let up = keyboard.feed(&[0; 8]);
        assert_eq!(up.as_slice(), &[0xaa, 0x9e]);
    }

    #[test]
    fn six_key_rollover_tracks_individual_keys() {
        let mut keyboard = KeyboardState::new();
        keyboard.feed(&[0, 0, 4, 5, 6, 7, 8, 9]);
        let changed = keyboard.feed(&[0, 0, 4, 6, 8, 10, 11, 12]);
        assert!(changed.as_slice().contains(&(0x30 | 0x80))); // b release
        assert!(changed.as_slice().contains(&(0x22 | 0x80))); // g release
        assert!(changed.as_slice().contains(&0x17)); // i press
        assert!(!changed.as_slice().contains(&0x1e)); // a remained down
    }

    #[test]
    fn rollover_error_does_not_release_held_keys() {
        let mut keyboard = KeyboardState::new();
        keyboard.feed(&[0, 0, 4, 0, 0, 0, 0, 0]);
        assert!(keyboard.feed(&[0, 0, 1, 1, 1, 1, 1, 1]).as_slice().is_empty());
        let released = keyboard.feed(&[0; 8]);
        assert_eq!(released.as_slice(), &[0x9e]);
    }

    #[test]
    fn relative_mouse_clamps_and_reports_button_transitions() {
        let mut mouse = MouseState::new();
        let press = mouse.feed(&[1, 120, 0], 100, 50).unwrap();
        assert_eq!((press.x, press.y), (99, 0));
        assert_eq!((press.buttons, press.changed), (1, 1));
        let release = mouse.feed(&[0, 0x9c, 0xfb], 100, 50).unwrap();
        assert_eq!((release.x, release.y), (0, 0));
        assert_eq!((release.buttons, release.changed), (0, 1));
        assert!(mouse.feed(&[0, 0, 0], 100, 50).is_none());
    }

    #[test]
    fn zero_bound_leaves_that_axis_unbounded() {
        let mut mouse = MouseState::new();
        let event = mouse.feed(&[0, 0xff, 0xff], 0, 0).unwrap();
        assert_eq!((event.x, event.y), (-1, -1));
    }
}
