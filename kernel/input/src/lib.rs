//! The PS/2 controller: keyboard scancodes and mouse packets.
//!
//! # What this crate does
//!
//! It turns the byte stream the PS/2 controller produces into *events*.
//!
//! Two decoders live here:
//!
//! * [`Keyboard`] — a scancode stream to key events, with the shift and
//!   caps-lock state that turns a scancode into a character.
//! * [`Mouse`] — the three-byte mouse packet to a position and a button
//!   state.
//!
//! # Why the decoders are separate from the hardware access
//!
//! The hardware access is a port read. The *decoding* is arithmetic over a
//! byte stream, and arithmetic over a byte stream is exactly what a unit
//! test can check exhaustively. So the decoders take bytes and produce
//! events, and the driver feeds them bytes.
//!
//! That split is not a convenience: the mouse packet's signed 16-bit deltas
//! are the classic place to get the sign wrong, and a sign error there moves
//! the pointer the wrong way — a bug that a test with a known packet catches
//! and a hardware test does not.

#![no_std]

extern crate alloc;

use alloc::collections::VecDeque;
use alloc::vec::Vec;
use spin::Mutex;

/// The PS/2 controller's I/O ports.
pub mod ports {
    /// The data port: the controller reads and writes here.
    pub const DATA: u16 = 0x60;
    /// The status port: the controller's state is here.
    pub const STATUS: u16 = 0x64;
    /// The controller's command port.
    pub const COMMAND: u16 = 0x64;
}

/// The status port's bits.
pub mod status {
    /// The controller has a byte for the host to read.
    pub const OUTPUT_FULL: u8 = 1 << 0;
    /// The controller's input buffer is full.
    pub const INPUT_FULL: u8 = 1 << 1;
    /// The command was inhibited.
    pub const INHIBITED: u8 = 1 << 4;
}

/// A key event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyEvent {
    /// The key's code, in VelaOS's own keycode space.
    pub code: KeyCode,
    /// True when the key was released rather than pressed.
    pub released: bool,
    /// True when a shift key was down when the key was pressed.
    pub shift: bool,
    /// True when a control key was down when the key was pressed.
    pub ctrl: bool,
    /// True when an alt key was down when the key was pressed.
    pub alt: bool,
}

/// A key's code.
///
/// VelaOS uses its own codes rather than the AmigaOS raw key codes, because
/// the AmigaOS codes are a 68k convention (bit 7 means "shifted") that does
/// not survive being carried through a syscall boundary unchanged.
///
/// The code is a plain number rather than an enum because the printable
/// ASCII range has to be representable: a fieldless enum can only name the
/// values it enumerates, and naming all ninety-five printable characters
/// would make the type noisy without making it safer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct KeyCode(u32);

impl KeyCode {
    /// The key that has no name.
    pub const UNKNOWN: KeyCode = KeyCode(0);
    /// The backspace key.
    pub const BACKSPACE: KeyCode = KeyCode(0x100);
    /// The tab key.
    pub const TAB: KeyCode = KeyCode(0x101);
    /// The enter key.
    pub const ENTER: KeyCode = KeyCode(0x102);
    /// The escape key.
    pub const ESCAPE: KeyCode = KeyCode(0x103);
    /// The delete key.
    pub const DELETE: KeyCode = KeyCode(0x104);
    /// The left arrow.
    pub const LEFT: KeyCode = KeyCode(0x110);
    /// The up arrow.
    pub const UP: KeyCode = KeyCode(0x111);
    /// The right arrow.
    pub const RIGHT: KeyCode = KeyCode(0x112);
    /// The down arrow.
    pub const DOWN: KeyCode = KeyCode(0x113);
    /// The home key.
    pub const HOME: KeyCode = KeyCode(0x114);
    /// The end key.
    pub const END: KeyCode = KeyCode(0x115);
    /// The page-up key.
    pub const PAGE_UP: KeyCode = KeyCode(0x116);
    /// The page-down key.
    pub const PAGE_DOWN: KeyCode = KeyCode(0x117);
    /// The left shift key.
    pub const LEFT_SHIFT: KeyCode = KeyCode(0x120);
    /// The right shift key.
    pub const RIGHT_SHIFT: KeyCode = KeyCode(0x121);
    /// The left control key.
    pub const LEFT_CTRL: KeyCode = KeyCode(0x122);
    /// The right control key.
    pub const RIGHT_CTRL: KeyCode = KeyCode(0x123);
    /// The left alt key.
    pub const LEFT_ALT: KeyCode = KeyCode(0x124);
    /// The right alt key.
    pub const RIGHT_ALT: KeyCode = KeyCode(0x125);

    /// The code for a printable ASCII character.
    ///
    /// The ASCII code *is* the key code, so the conversion is arithmetic
    /// rather than a table lookup — a table would have to be searched, and
    /// this has to work in a `const` context.
    pub const fn from_ascii(ch: char) -> Self {
        let code = ch as u32;
        if code >= 0x20 && code < 0x7F {
            KeyCode(code)
        } else {
            Self::UNKNOWN
        }
    }

    /// The ASCII character this key produces, if any.
    pub const fn to_ascii(self) -> Option<char> {
        let code = self.0;
        if code >= 0x20 && code < 0x7F {
            // SAFETY: the range check guarantees the value is a valid ASCII
            // code point.
            Some(unsafe { core::char::from_u32_unchecked(code) })
        } else {
            None
        }
    }

    /// The raw code.
    pub const fn raw(self) -> u32 {
        self.0
    }

    /// A key code from its raw value.
    pub const fn from_raw(raw: u32) -> Self {
        KeyCode(raw)
    }
}

/// The scancode set 1 translation table.
///
/// Set 1 is the original PC scancode set, and it is what a PS/2 keyboard
/// sends. The table is indexed by scancode and gives the key's code; the
/// shifted variant is a second table.
///
/// A scancode's high bit is the "released" flag, so only the low seven bits
/// index the table.
mod scancodes {
    use super::KeyCode;

    /// The unshifted mapping.
    ///
    /// The `None` entries are keys that have no unshifted character — the
    /// function keys, the arrows, the modifiers — and which therefore need
    /// their own codes.
    pub const fn unshifted(code: u8) -> Option<KeyCode> {
        Some(match code {
            0x01 => KeyCode::ESCAPE,
            0x0E => KeyCode::BACKSPACE,
            0x0F => KeyCode::TAB,
            0x1C => KeyCode::ENTER,
            0x1D => KeyCode::LEFT_CTRL,
            0x2A => KeyCode::LEFT_SHIFT,
            0x38 => KeyCode::LEFT_ALT,
            0x39 => KeyCode::from_ascii(' '),
            0x3B => KeyCode::RIGHT_ALT,
            0x3D => KeyCode::RIGHT_CTRL,
            0x48 => KeyCode::UP,
            0x4B => KeyCode::LEFT,
            0x4D => KeyCode::RIGHT,
            0x50 => KeyCode::DOWN,
            0x47 => KeyCode::HOME,
            0x4F => KeyCode::END,
            0x49 => KeyCode::PAGE_UP,
            0x51 => KeyCode::PAGE_DOWN,
            0x53 => KeyCode::DELETE,
            0x2B => KeyCode::UNKNOWN,
            c if c >= 0x02 && c <= 0x0D => {
                // The number row, unshifted.
                KeyCode::from_ascii((b'1' + c - 0x02) as char)
            }
            c if c >= 0x10 && c <= 0x19 => {
                // `QWERTY`, and the codes continue from `Q`.
                KeyCode::from_ascii((b'Q' + c - 0x10) as char)
            }
            c if c >= 0x1E && c <= 0x26 => {
                // `ASDF...`, from `A`.
                KeyCode::from_ascii((b'A' + c - 0x1E) as char)
            }
            c if c >= 0x2C && c <= 0x32 => {
                // `ZXCV...`, from `Z`.
                KeyCode::from_ascii((b'Z' + c - 0x2C) as char)
            }
            _ => return None,
        })
    }

    /// The shifted mapping.
    pub const fn shifted(code: u8) -> Option<KeyCode> {
        Some(match code {
            0x0E => KeyCode::BACKSPACE,
            0x0F => KeyCode::TAB,
            0x1C => KeyCode::ENTER,
            0x39 => KeyCode::from_ascii(' '),
            c if c >= 0x02 && c <= 0x0D => {
                // The number row's shifted characters: `!@#$%^&*()_+`.
                const SHIFTED: &[u8] = b"!@#$%^&*()_+";
                KeyCode::from_ascii(SHIFTED[(c - 0x02) as usize] as char)
            }
            c if c >= 0x10 && c <= 0x19 => {
                // The letter row's shifted characters.
                const SHIFTED: &[u8] = b"QWERTYUIOP{}";
                KeyCode::from_ascii(SHIFTED[(c - 0x10) as usize] as char)
            }
            c if c >= 0x1E && c <= 0x26 => {
                const SHIFTED: &[u8] = b"ASDFGHJKL:\"";
                KeyCode::from_ascii(SHIFTED[(c - 0x1E) as usize] as char)
            }
            c if c >= 0x2C && c <= 0x32 => {
                const SHIFTED: &[u8] = b"ZXCVBNM<>?";
                KeyCode::from_ascii(SHIFTED[(c - 0x2C) as usize] as char)
            }
            _ => return None,
        })
    }
}

/// The keyboard decoder.
///
/// It consumes scancodes and produces key events. The shift, control and
/// alt state is tracked here, because it is the decoder that knows which
/// scancode is a modifier.
#[derive(Debug, Clone, Default)]
pub struct Keyboard {
    /// Whether a shift key is down.
    shift: bool,
    /// Whether a control key is down.
    ctrl: bool,
    /// Whether an alt key is down.
    alt: bool,
    /// Whether caps lock is on.
    caps: bool,
}

impl Keyboard {
    /// A fresh decoder, with every modifier released.
    pub const fn new() -> Self {
        Self {
            shift: false,
            ctrl: false,
            alt: false,
            caps: false,
        }
    }

    /// Whether caps lock is on.
    pub const fn caps_lock(&self) -> bool {
        self.caps
    }

    /// Whether shift is down.
    pub const fn shift(&self) -> bool {
        self.shift
    }

    /// Whether control is down.
    pub const fn ctrl(&self) -> bool {
        self.ctrl
    }

    /// Whether alt is down.
    pub const fn alt(&self) -> bool {
        self.alt
    }

    /// Feed one scancode.
    ///
    /// Returns the event it produced, if any.
    ///
    /// A scancode with its high bit set is a *release*. The release of a
    /// modifier updates the modifier state and produces no event of its own:
    /// a program that tracked the release of shift would have to know that
    /// shift is a modifier, and the decoder is the party that knows.
    pub fn feed(&mut self, scancode: u8) -> Option<KeyEvent> {
        let released = scancode & 0x80 != 0;
        let code = scancode & 0x7F;

        // The modifier keys update the state first, so that the *next* key
        // sees the new state.
        //
        // The set 1 scancodes for the modifiers are:
        //
        // | Code | Key |
        // |---|---|
        // | `0x1D` | left control |
        // | `0x2A` | left shift |
        // | `0x36` | right shift |
        // | `0x38` | left alt |
        // | `0x3A` | caps lock |
        // | `0x3B` | right alt |
        // | `0x3D` | right control |
        match code {
            0x2A | 0x36 => {
                self.shift = !released;
                return None;
            }
            0x1D | 0x3D => {
                self.ctrl = !released;
                return None;
            }
            0x38 | 0x3B => {
                self.alt = !released;
                return None;
            }
            0x3A => {
                // Caps lock toggles on press, and produces no event of its
                // own. The release is ignored: a lock that toggled on the
                // release would toggle twice.
                if !released {
                    self.caps = !self.caps;
                }
                return None;
            }
            _ => {}
        }

        // The key's code, taking shift into account. Caps lock affects only
        // the letter keys — that is what a caps lock *is*.
        let base = scancodes::unshifted(code);
        let shifted = scancodes::shifted(code);
        let Some(base) = base else {
            // A key with no mapping in either table produces no event: an
            // unknown key is not an error, it is simply not interesting.
            return None;
        };
        let is_letter = base
            .to_ascii()
            .map(|c| c.is_ascii_alphabetic())
            .unwrap_or(false);
        let final_code = if is_letter {
            // A letter's case is decided by shift *or* caps lock, but not by
            // both at once — which is the behaviour every keyboard has.
            let upper = self.shift ^ self.caps;
            if upper {
                match base.to_ascii() {
                    Some(c) => KeyCode::from_ascii(c.to_ascii_uppercase()),
                    None => base,
                }
            } else {
                base
            }
        } else if self.shift {
            shifted.unwrap_or(base)
        } else {
            base
        };

        Some(KeyEvent {
            code: final_code,
            released,
            shift: self.shift,
            ctrl: self.ctrl,
            alt: self.alt,
        })
    }
}

/// A mouse event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MouseEvent {
    /// The pointer's new position, in screen coordinates.
    pub x: i32,
    /// The pointer's new position, in screen coordinates.
    pub y: i32,
    /// The buttons that are down.
    ///
    /// The bits are the AmigaOS ones: bit 0 is the left button, bit 1 the
    /// right, bit 2 the middle.
    pub buttons: u8,
    /// The buttons that *changed* in this event.
    pub changed: u8,
}

/// The mouse's buttons, as the AmigaOS sees them.
pub mod buttons {
    /// The left button.
    pub const LEFT: u8 = 1 << 0;
    /// The right button.
    pub const RIGHT: u8 = 1 << 1;
    /// The middle button.
    pub const MIDDLE: u8 = 1 << 2;
}

/// The mouse decoder.
///
/// The PS/2 mouse sends a three-byte packet: the first byte has the button
/// state and the sign bits of the two deltas, the second is the x delta and
/// the third is the y delta. The deltas are *signed* — the sign lives in the
/// first byte, not in the delta bytes themselves.
#[derive(Debug, Clone, Default)]
pub struct Mouse {
    /// The pointer's position.
    x: i32,
    /// The pointer's position.
    y: i32,
    /// The buttons that are down.
    buttons: u8,
    /// The bytes of the packet received so far.
    packet: [u8; 3],
    /// How many bytes of the packet have been received.
    filled: usize,
    /// The screen's bounds, which the pointer is clamped to.
    bounds: (i32, i32, i32, i32),
}

impl Mouse {
    /// A fresh decoder, with the pointer at the origin.
    pub const fn new() -> Self {
        Self {
            x: 0,
            y: 0,
            buttons: 0,
            packet: [0; 3],
            filled: 0,
            bounds: (0, 0, 0, 0),
        }
    }

    /// Set the screen's bounds, so the pointer is clamped to it.
    ///
    /// A width or height of zero means "unbounded" in that axis.
    pub fn set_bounds(&mut self, x: i32, y: i32, width: i32, height: i32) {
        self.bounds = (x, y, width, height);
    }

    /// The pointer's position.
    pub const fn position(&self) -> (i32, i32) {
        (self.x, self.y)
    }

    /// The buttons that are down.
    pub const fn buttons(&self) -> u8 {
        self.buttons
    }

    /// The number of bytes buffered for the current packet.
    pub const fn pending(&self) -> usize {
        self.filled
    }

    /// Feed one byte.
    ///
    /// Returns the event the completed packet produced, if any.
    pub fn feed(&mut self, byte: u8) -> Option<MouseEvent> {
        self.packet[self.filled] = byte;
        self.filled += 1;
        if self.filled < 3 {
            return None;
        }
        self.filled = 0;

        let first = self.packet[0];
        // The first byte always has bit 3 set; a packet that does not is a
        // resynchronisation error, and the packet is discarded.
        if first & 0x08 == 0 {
            return None;
        }

        // The deltas are signed, and the sign is in the first byte. The
        // arithmetic below is *unsigned* on purpose: the ninth bit is the
        // sign, so the value is a nine-bit two's-complement number whose
        // top bit is bit 0 of the first byte.
        let dx = sign_extend_9(self.packet[1], first & 0x01 != 0);
        // The y axis grows downwards, so the sign bit is bit 4 of the first
        // byte and the delta is negated relative to the "up is positive"
        // convention the hardware uses.
        let dy = sign_extend_9(self.packet[2], first & 0x10 != 0);

        let new_buttons = first & 0x07;
        let changed = new_buttons ^ self.buttons;
        self.buttons = new_buttons;

        self.x = self.clamp_x(self.x + dx);
        self.y = self.clamp_y(self.y + dy);

        Some(MouseEvent {
            x: self.x,
            y: self.y,
            buttons: self.buttons,
            changed,
        })
    }

    /// Clamp an x coordinate to the bounds.
    fn clamp_x(&self, x: i32) -> i32 {
        let (bx, _, width, _) = self.bounds;
        if width <= 0 {
            return x;
        }
        x.clamp(bx, bx + width - 1)
    }

    /// Clamp a y coordinate to the bounds.
    fn clamp_y(&self, y: i32) -> i32 {
        let (_, by, _, height) = self.bounds;
        if height <= 0 {
            return y;
        }
        y.clamp(by, by + height - 1)
    }
}

/// Sign-extend a byte whose ninth bit is `negative`.
///
/// The value is a nine-bit two's-complement number: eight bits of magnitude
/// and a sign bit that lives elsewhere. The extension is what makes a delta
/// of `0xFF` with the sign bit set mean `-1` rather than `255`.
fn sign_extend_9(byte: u8, negative: bool) -> i32 {
    if negative {
        // `!byte` on a `u8` is already the eight-bit complement, so the
        // `& 0xFF` that a naive version needs is redundant: the type carries
        // the width. Negating the complement gives the magnitude, and the
        // outer sign gives the two's-complement value.
        -((!byte) as i32 + 1)
    } else {
        byte as i32
    }
}

/// The input event queue.
///
/// The queue is what the kernel's interrupt handler writes to and what the
/// Intuition service reads from. It is a bounded ring: an input device that
/// outruns the consumer must not be able to make the kernel allocate without
/// limit.
pub struct EventQueue {
    events: Mutex<VecDeque<Event>>,
    capacity: usize,
}

/// An input event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    /// A key was pressed or released.
    Key(KeyEvent),
    /// The mouse moved or a button changed.
    Mouse(MouseEvent),
}

/// The default queue capacity.
///
/// 256 events is enough that a burst of pointer movement does not overflow
/// it, and small enough that the queue's memory is a rounding error.
pub const DEFAULT_QUEUE_CAPACITY: usize = 256;

impl EventQueue {
    /// A queue with the given capacity.
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            events: Mutex::new(VecDeque::new()),
            capacity,
        }
    }

    /// A queue with the default capacity.
    pub fn new() -> Self {
        Self::with_capacity(DEFAULT_QUEUE_CAPACITY)
    }

    /// Push an event.
    ///
    /// Returns `false` when the queue was full and the event was dropped. A
    /// dropped event is a real loss, but it is a bounded one: the alternative
    /// — growing without limit — turns a slow consumer into a kernel
    /// out-of-memory condition.
    pub fn push(&self, event: Event) -> bool {
        let mut events = self.events.lock();
        if events.len() >= self.capacity {
            return false;
        }
        events.push_back(event);
        true
    }

    /// Take the oldest event.
    pub fn pop(&self) -> Option<Event> {
        self.events.lock().pop_front()
    }

    /// Take every event, in order.
    pub fn drain(&self) -> Vec<Event> {
        let mut events = self.events.lock();
        let out: Vec<Event> = events.drain(..).collect();
        out
    }

    /// How many events are queued.
    pub fn len(&self) -> usize {
        self.events.lock().len()
    }

    /// True when the queue is empty.
    pub fn is_empty(&self) -> bool {
        self.events.lock().is_empty()
    }

    /// The queue's capacity.
    pub const fn capacity(&self) -> usize {
        self.capacity
    }
}

impl core::fmt::Debug for EventQueue {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("EventQueue")
            .field("queued", &self.len())
            .field("capacity", &self.capacity)
            .finish()
    }
}

impl Default for EventQueue {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
extern crate std;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_printable_key_produces_its_ascii_code() {
        let mut kb = Keyboard::new();
        // Scancode 0x1E is `A` in set 1.
        let event = kb.feed(0x1E).unwrap();
        assert_eq!(event.code, KeyCode::from_ascii('A'));
        assert!(!event.released);
    }

    #[test]
    fn a_released_key_has_the_high_bit_set() {
        let mut kb = Keyboard::new();
        let event = kb.feed(0x9E).unwrap();
        assert!(event.released);
        assert_eq!(event.code, KeyCode::from_ascii('A'));
    }

    #[test]
    fn shift_changes_the_character() {
        let mut kb = Keyboard::new();
        // Shift down.
        assert!(kb.feed(0x2A).is_none());
        assert!(kb.shift());
        let event = kb.feed(0x1E).unwrap();
        assert_eq!(event.code, KeyCode::from_ascii('A'));
        assert!(event.shift);
    }

    #[test]
    fn a_modifier_produces_no_event_of_its_own() {
        let mut kb = Keyboard::new();
        assert!(kb.feed(0x2A).is_none());
        assert!(kb.feed(0x1D).is_none());
        assert!(kb.feed(0x38).is_none());
        assert!(kb.shift() && kb.ctrl() && kb.alt());
    }

    #[test]
    fn a_modifier_release_updates_the_state() {
        let mut kb = Keyboard::new();
        kb.feed(0x2A);
        assert!(kb.shift());
        kb.feed(0xAA);
        assert!(!kb.shift());
    }

    #[test]
    fn caps_lock_uppercases_letters_but_not_digits() {
        let mut kb = Keyboard::new();
        // Caps lock toggles on press.
        kb.feed(0x3A);
        assert!(kb.caps_lock());
        // `a` is scancode 0x1E shifted to lowercase; the set 1 table only
        // carries the unshifted letter, so caps lock is what uppercases it.
        let event = kb.feed(0x1E).unwrap();
        assert_eq!(event.code, KeyCode::from_ascii('A'));
    }

    #[test]
    fn the_number_row_has_its_shifted_characters() {
        let mut kb = Keyboard::new();
        kb.feed(0x2A);
        // Scancode 0x02 is `1`, shifted it is `!`.
        let event = kb.feed(0x02).unwrap();
        assert_eq!(event.code, KeyCode::from_ascii('!'));
    }

    #[test]
    fn an_unknown_scancode_produces_no_event() {
        let mut kb = Keyboard::new();
        assert!(kb.feed(0x77).is_none());
    }

    #[test]
    fn a_keycode_round_trips_through_its_ascii_form() {
        for c in ' '..='~' {
            let code = KeyCode::from_ascii(c);
            assert_eq!(code.to_ascii(), Some(c));
        }
        assert_eq!(KeyCode::from_ascii('\n').to_ascii(), None);
        assert_eq!(KeyCode::UNKNOWN.to_ascii(), None);
    }

    #[test]
    fn a_mouse_packet_moves_the_pointer() {
        let mut mouse = Mouse::new();
        // The first byte always has bit 3 set.
        assert!(mouse.feed(0x08).is_none());
        assert!(mouse.feed(0x05).is_none());
        let event = mouse.feed(0x00).unwrap();
        assert_eq!(event.x, 5);
        assert_eq!(event.y, 0);
    }

    #[test]
    fn a_negative_x_delta_moves_the_pointer_left() {
        // This is the classic sign bug: the delta byte is 0xFF and the sign
        // bit is in the first byte.
        let mut mouse = Mouse::new();
        mouse.feed(0x09);
        mouse.feed(0xFF);
        let event = mouse.feed(0x00).unwrap();
        assert_eq!(event.x, -1, "a negative delta must be negative");
    }

    #[test]
    fn a_negative_y_delta_moves_the_pointer_up() {
        let mut mouse = Mouse::new();
        // Bit 4 of the first byte is the y sign bit.
        mouse.feed(0x18);
        mouse.feed(0x00);
        let event = mouse.feed(0xFF).unwrap();
        assert_eq!(event.y, -1);
    }

    #[test]
    fn a_full_negative_delta_is_minus_one_not_minus_two_hundred_fifty_five() {
        let mut mouse = Mouse::new();
        mouse.feed(0x09);
        mouse.feed(0xFF);
        let event = mouse.feed(0x00).unwrap();
        // The sign-extended value is `-1`, not `-255` and not `255`.
        assert_eq!(event.x, -1);
    }

    #[test]
    fn a_button_change_is_reported() {
        let mut mouse = Mouse::new();
        mouse.feed(0x09); // Left button down.
        mouse.feed(0x00);
        let event = mouse.feed(0x00).unwrap();
        assert_eq!(event.buttons, buttons::LEFT);
        assert_eq!(event.changed, buttons::LEFT);
        assert_eq!(mouse.buttons(), buttons::LEFT);
    }

    #[test]
    fn an_unchanged_button_reports_no_change() {
        let mut mouse = Mouse::new();
        mouse.feed(0x09); // Left button down.
        mouse.feed(0x00);
        mouse.feed(0x00);
        // A second packet with the same button state and no movement.
        mouse.feed(0x09);
        mouse.feed(0x00);
        let event = mouse.feed(0x00).unwrap();
        assert_eq!(event.buttons, buttons::LEFT);
        assert_eq!(event.changed, 0);
    }

    #[test]
    fn a_resynchronisation_error_discards_the_packet() {
        let mut mouse = Mouse::new();
        // Bit 3 clear means the packet is out of sync.
        mouse.feed(0x00);
        mouse.feed(0x00);
        assert!(mouse.feed(0x00).is_none());
        assert_eq!(mouse.position(), (0, 0));
    }

    #[test]
    fn the_pointer_is_clamped_to_the_screen() {
        let mut mouse = Mouse::new();
        mouse.set_bounds(0, 0, 100, 50);
        // A large positive delta.
        mouse.feed(0x08);
        mouse.feed(0x40);
        let event = mouse.feed(0x00).unwrap();
        assert_eq!(event.x, 64);
        // A large negative delta.
        mouse.feed(0x09);
        mouse.feed(0xC0);
        let event = mouse.feed(0x00).unwrap();
        assert_eq!(event.x, 0);
        assert!(event.x >= 0 && event.x < 100);
    }

    #[test]
    fn the_pointer_is_clamped_to_the_lower_bound_too() {
        let mut mouse = Mouse::new();
        mouse.set_bounds(10, 20, 100, 50);
        mouse.feed(0x09);
        mouse.feed(0xFF);
        let event = mouse.feed(0x00).unwrap();
        assert_eq!(event.x, 10);
        assert_eq!(event.y, 20);
    }

    #[test]
    fn an_unbounded_screen_does_not_clamp() {
        let mut mouse = Mouse::new();
        mouse.set_bounds(0, 0, 0, 0);
        mouse.feed(0x08);
        mouse.feed(0x40);
        let event = mouse.feed(0x00).unwrap();
        assert_eq!(event.x, 64);
    }

    #[test]
    fn the_event_queue_keeps_its_order() {
        let q = EventQueue::with_capacity(8);
        assert!(q.push(Event::Key(KeyEvent {
            code: KeyCode::from_ascii('a'),
            released: false,
            shift: false,
            ctrl: false,
            alt: false,
        })));
        assert!(q.push(Event::Key(KeyEvent {
            code: KeyCode::from_ascii('b'),
            released: false,
            shift: false,
            ctrl: false,
            alt: false,
        })));
        assert_eq!(q.len(), 2);
        match q.pop().unwrap() {
            Event::Key(e) => assert_eq!(e.code, KeyCode::from_ascii('a')),
            _ => panic!("expected a key event"),
        }
        match q.pop().unwrap() {
            Event::Key(e) => assert_eq!(e.code, KeyCode::from_ascii('b')),
            _ => panic!("expected a key event"),
        }
        assert!(q.is_empty());
    }

    #[test]
    fn the_event_queue_is_bounded() {
        let q = EventQueue::with_capacity(2);
        let event = Event::Key(KeyEvent {
            code: KeyCode::UNKNOWN,
            released: false,
            shift: false,
            ctrl: false,
            alt: false,
        });
        assert!(q.push(event));
        assert!(q.push(event));
        // The third event does not fit, and is refused rather than growing
        // the queue.
        assert!(!q.push(event));
        assert_eq!(q.len(), 2);
    }

    #[test]
    fn draining_the_queue_returns_everything_in_order() {
        let q = EventQueue::new();
        for i in 0..5u32 {
            q.push(Event::Key(KeyEvent {
                code: KeyCode::from_ascii((b'a' + i as u8) as char),
                released: false,
                shift: false,
                ctrl: false,
                alt: false,
            }));
        }
        let events = q.drain();
        assert_eq!(events.len(), 5);
        assert!(q.is_empty());
        for (i, event) in events.iter().enumerate() {
            match event {
                Event::Key(e) => assert_eq!(e.code, KeyCode::from_ascii((b'a' + i as u8) as char)),
                _ => panic!("expected a key event"),
            }
        }
    }
}
