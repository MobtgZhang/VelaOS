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
    /// Keyboard LED state bitmask: bit 0 = caps lock, bit 1 = num lock,
    /// bit 2 = scroll lock. The decoder tracks lock toggles and publishes
    /// them with every key event so the desktop can show keyboard LEDs.
    pub leds: u8,
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

/// The scancode set 1 translation tables.
///
/// Set 1 is the original PC scancode set, and it is what the guest receives:
/// the keyboard itself speaks set 2, and the controller's translator — which
/// the firmware leaves enabled — converts before the byte reaches port
/// `0x60`. The tables are indexed by scancode and give the key's code.
///
/// A scancode's high bit is the "released" flag, so only the low seven bits
/// index the table.
///
/// # Why the rows are strings rather than arithmetic
///
/// The letter rows look temptingly regular — scancodes `0x10..=0x19` are
/// consecutive, so `b'Q' + code - 0x10` looks like the top row. It is not:
/// the *scancodes* are consecutive but the *letters* are not, because the
/// rows spell QWERTY rather than the alphabet. `b'Q' + 1` is `R`, not `W`,
/// and a decoder built on the arithmetic types `RST` for `qwe`, `BCD` for
/// `asd`, and `:;` for `0-` — every one of them a plausible-looking,
/// completely wrong character. (Only the bottom row gets away with the
/// arithmetic by accident, and only as far as `n`.) Every row is therefore a
/// literal string indexed by the scancode's offset, so the rows are spelled
/// out and cannot drift.
mod scancodes {
    use super::KeyCode;

    /// The number row's unshifted characters, scancodes `0x02..=0x0B`.
    ///
    /// The row has twelve keys but only ten digits: the last two keys are
    /// `-` and `=`, which have their own arms below.
    const NUMBERS: &[u8] = b"1234567890";

    /// The top letter row, scancodes `0x10..=0x19`.
    const TOP_ROW: &[u8] = b"qwertyuiop";

    /// The home letter row, scancodes `0x1E..=0x26`.
    const HOME_ROW: &[u8] = b"asdfghjkl";

    /// The bottom letter row, scancodes `0x2C..=0x32`.
    const BOTTOM_ROW: &[u8] = b"zxcvbnm";

    /// The number row's shifted characters, scancodes `0x02..=0x0D`.
    const SHIFTED_NUMBERS: &[u8] = b"!@#$%^&*()_+";

    /// The top letter row's shifted characters, scancodes `0x10..=0x19`.
    const SHIFTED_TOP_ROW: &[u8] = b"QWERTYUIOP";

    /// The home letter row's shifted characters, scancodes `0x1E..=0x26`.
    const SHIFTED_HOME_ROW: &[u8] = b"ASDFGHJKL";

    /// The bottom letter row's shifted characters, scancodes `0x2C..=0x32`.
    const SHIFTED_BOTTOM_ROW: &[u8] = b"ZXCVBNM";

    /// A byte's key code, for the character tables above.
    const fn ascii(ch: u8) -> KeyCode {
        KeyCode::from_ascii(ch as char)
    }

    /// The unshifted mapping: the character a key types on its own.
    ///
    /// The letters are the *lowercase* ones — an unshifted key types
    /// lowercase, and the decoder's shift and caps state decides the case.
    pub const fn unshifted(code: u8) -> Option<KeyCode> {
        Some(match code {
            0x01 => KeyCode::ESCAPE,
            0x0E => KeyCode::BACKSPACE,
            0x0F => KeyCode::TAB,
            0x1C => KeyCode::ENTER,
            0x1D => KeyCode::LEFT_CTRL,
            0x2A => KeyCode::LEFT_SHIFT,
            0x36 => KeyCode::RIGHT_SHIFT,
            0x38 => KeyCode::LEFT_ALT,
            0x39 => KeyCode::from_ascii(' '),
            // The number row's last two keys, which the digit string above
            // does not cover.
            0x0C => KeyCode::from_ascii('-'),
            0x0D => KeyCode::from_ascii('='),
            // The punctuation keys, which sit between and beside the letter
            // rows and which the rows above skip.
            0x1A => KeyCode::from_ascii('['),
            0x1B => KeyCode::from_ascii(']'),
            0x27 => KeyCode::from_ascii(';'),
            0x28 => KeyCode::from_ascii('\''),
            0x29 => KeyCode::from_ascii('`'),
            0x2B => KeyCode::from_ascii('\\'),
            0x33 => KeyCode::from_ascii(','),
            0x34 => KeyCode::from_ascii('.'),
            0x35 => KeyCode::from_ascii('/'),
            // The keypad's operators, which are independent of NumLock.
            0x37 => KeyCode::from_ascii('*'),
            0x4A => KeyCode::from_ascii('-'),
            0x4E => KeyCode::from_ascii('+'),
            // The navigation cluster and the keypad's home-row doubles. The
            // extended (0xE0-prefixed) forms arrive as these same codes
            // because the prefix byte is not a scancode the decoder reads.
            0x47 => KeyCode::HOME,
            0x48 => KeyCode::UP,
            0x49 => KeyCode::PAGE_UP,
            0x4B => KeyCode::LEFT,
            0x4D => KeyCode::RIGHT,
            0x4F => KeyCode::END,
            0x50 => KeyCode::DOWN,
            0x51 => KeyCode::PAGE_DOWN,
            0x53 => KeyCode::DELETE,
            c if c >= 0x02 && c <= 0x0B => ascii(NUMBERS[(c - 0x02) as usize]),
            c if c >= 0x10 && c <= 0x19 => ascii(TOP_ROW[(c - 0x10) as usize]),
            c if c >= 0x1E && c <= 0x26 => ascii(HOME_ROW[(c - 0x1E) as usize]),
            c if c >= 0x2C && c <= 0x32 => ascii(BOTTOM_ROW[(c - 0x2C) as usize]),
            _ => return None,
        })
    }

    /// The shifted mapping: the character a key types with shift held.
    ///
    /// The letter rows give the uppercase letters, so a letter's case is a
    /// table lookup rather than an arithmetic case-fold.
    pub const fn shifted(code: u8) -> Option<KeyCode> {
        Some(match code {
            0x0E => KeyCode::BACKSPACE,
            0x0F => KeyCode::TAB,
            0x1C => KeyCode::ENTER,
            0x39 => KeyCode::from_ascii(' '),
            // The punctuation keys' shifted characters.
            0x1A => KeyCode::from_ascii('{'),
            0x1B => KeyCode::from_ascii('}'),
            0x27 => KeyCode::from_ascii(':'),
            0x28 => KeyCode::from_ascii('"'),
            0x29 => KeyCode::from_ascii('~'),
            0x2B => KeyCode::from_ascii('|'),
            0x33 => KeyCode::from_ascii('<'),
            0x34 => KeyCode::from_ascii('>'),
            0x35 => KeyCode::from_ascii('?'),
            c if c >= 0x02 && c <= 0x0D => ascii(SHIFTED_NUMBERS[(c - 0x02) as usize]),
            c if c >= 0x10 && c <= 0x19 => ascii(SHIFTED_TOP_ROW[(c - 0x10) as usize]),
            c if c >= 0x1E && c <= 0x26 => ascii(SHIFTED_HOME_ROW[(c - 0x1E) as usize]),
            c if c >= 0x2C && c <= 0x32 => ascii(SHIFTED_BOTTOM_ROW[(c - 0x2C) as usize]),
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
    /// Keyboard LED state bitmask: bit 0 = caps lock, bit 1 = num lock,
    /// bit 2 = scroll lock.
    leds: u8,
}

impl Keyboard {
    /// A fresh decoder, with every modifier released.
    pub const fn new() -> Self {
        Self {
            shift: false,
            ctrl: false,
            alt: false,
            leds: 0,
        }
    }

    /// Whether caps lock is on.
    pub const fn caps_lock(&self) -> bool {
        self.leds & 1 != 0
    }

    /// Whether num lock is on.
    pub const fn num_lock(&self) -> bool {
        self.leds & 2 != 0
    }

    /// Whether scroll lock is on.
    pub const fn scroll_lock(&self) -> bool {
        self.leds & 4 != 0
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
        //
        // The right-hand control and alt keys are the extended `0xE0`
        // prefixes' partners (`0xE0 0x1D`, `0xE0 0x38`); the prefix byte
        // itself decodes to nothing, so their partner byte lands here and
        // they work through the same arms.
        match code {
            0x2A | 0x36 => {
                self.shift = !released;
                return None;
            }
            0x1D => {
                self.ctrl = !released;
                return None;
            }
            0x38 => {
                self.alt = !released;
                return None;
            }
            0x3A => {
                // Caps lock toggles on press, and produces no event of its
                // own. The release is ignored: a lock that toggled on the
                // release would toggle twice.
                if !released {
                    self.leds ^= 1;
                }
                return None;
            }
            0x45 => {
                // Num lock toggles on press.
                if !released {
                    self.leds ^= 2;
                }
                return None;
            }
            0x46 => {
                // Scroll lock toggles on press.
                if !released {
                    self.leds ^= 4;
                }
                return None;
            }
            _ => {}
        }

        // The key's code, taking shift into account. Caps lock affects only
        // the letter keys — that is what a caps lock *is*.
        let Some(base) = scancodes::unshifted(code) else {
            // A key with no mapping in either table produces no event: an
            // unknown key is not an error, it is simply not interesting.
            return None;
        };
        // The unshifted table hands out lowercase letters, so a letter is
        // recognisable by its base being one. A letter's case is decided by
        // shift *or* caps lock, but not by both at once — which is the
        // behaviour every keyboard has — and the case itself comes from the
        // shifted table, so the tables stay the only source of the mapping.
        let is_letter = base
            .to_ascii()
            .map(|c| c.is_ascii_lowercase())
            .unwrap_or(false);
        let final_code = if is_letter {
            if self.shift ^ (self.leds & 1 != 0) {
                scancodes::shifted(code).unwrap_or(base)
            } else {
                base
            }
        } else if self.shift {
            scancodes::shifted(code).unwrap_or(base)
        } else {
            base
        };

        Some(KeyEvent {
            code: final_code,
            released,
            shift: self.shift,
            ctrl: self.ctrl,
            alt: self.alt,
            leds: self.leds,
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
    pub const fn with_capacity(capacity: usize) -> Self {
        Self {
            events: Mutex::new(VecDeque::new()),
            capacity,
        }
    }

    /// A queue with the default capacity.
    pub const fn new() -> Self {
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

    /// Borrow the newest event, for a producer coalescing pointer motion.
    pub fn back_mut(&mut self) -> Option<&mut Event> {
        self.events.get_mut().back_mut()
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
        // Scancode 0x1E is `A` in set 1, and an unshifted `A` types a
        // lowercase `a`.
        let event = kb.feed(0x1E).unwrap();
        assert_eq!(event.code, KeyCode::from_ascii('a'));
        assert!(!event.released);
    }

    #[test]
    fn a_released_key_has_the_high_bit_set() {
        let mut kb = Keyboard::new();
        let event = kb.feed(0x9E).unwrap();
        assert!(event.released);
        assert_eq!(event.code, KeyCode::from_ascii('a'));
    }

    #[test]
    fn the_letter_rows_spell_qwerty_not_the_alphabet() {
        let mut kb = Keyboard::new();
        // The rows are the trap this table was rebuilt to avoid: the
        // scancodes are consecutive but the letters are not, and an
        // arithmetic row types `RST` for `qwe`.
        for (offset, expected) in b"qwertyuiop".iter().enumerate() {
            let event = kb.feed(0x10 + offset as u8).unwrap();
            assert_eq!(
                event.code,
                KeyCode::from_ascii(*expected as char),
                "top row, offset {offset}"
            );
        }
        for (offset, expected) in b"asdfghjkl".iter().enumerate() {
            let event = kb.feed(0x1E + offset as u8).unwrap();
            assert_eq!(
                event.code,
                KeyCode::from_ascii(*expected as char),
                "home row, offset {offset}"
            );
        }
        for (offset, expected) in b"zxcvbnm".iter().enumerate() {
            let event = kb.feed(0x2C + offset as u8).unwrap();
            assert_eq!(
                event.code,
                KeyCode::from_ascii(*expected as char),
                "bottom row, offset {offset}"
            );
        }
    }

    #[test]
    fn the_number_row_ends_with_zero_dash_and_equals() {
        let mut kb = Keyboard::new();
        // `0` sits after `9`, not after the character `9`'s arithmetic
        // successor would suggest, and the row's last two keys are the
        // punctuation `-` and `=`.
        let expected = b"1234567890-=";
        for (offset, expected) in expected.iter().enumerate() {
            let event = kb.feed(0x02 + offset as u8).unwrap();
            assert_eq!(
                event.code,
                KeyCode::from_ascii(*expected as char),
                "number row, offset {offset}"
            );
        }
    }

    #[test]
    fn the_punctuation_keys_type_their_characters() {
        let mut kb = Keyboard::new();
        // The unshifted character, then the shifted one.
        let pairs: &[(u8, char, char)] = &[
            (0x1A, '[', '{'),
            (0x1B, ']', '}'),
            (0x27, ';', ':'),
            (0x28, '\'', '"'),
            (0x29, '`', '~'),
            (0x2B, '\\', '|'),
            (0x33, ',', '<'),
            (0x34, '.', '>'),
            (0x35, '/', '?'),
        ];
        for &(code, unshifted, shifted) in pairs {
            let event = kb.feed(code).unwrap();
            assert_eq!(
                event.code,
                KeyCode::from_ascii(unshifted),
                "scancode {code:#04x} unshifted"
            );
            assert!(kb.feed(0x2A).is_none(), "shift down for {code:#04x}");
            let event = kb.feed(code).unwrap();
            assert_eq!(
                event.code,
                KeyCode::from_ascii(shifted),
                "scancode {code:#04x} shifted"
            );
            assert!(kb.feed(0xAA).is_none(), "shift up after {code:#04x}");
        }
    }

    #[test]
    fn shift_and_caps_together_type_lowercase() {
        let mut kb = Keyboard::new();
        // Shift *or* caps decides the case, not both: holding shift under a
        // lit caps lock is how a keyboard types the lowercase letter back.
        assert!(kb.feed(0x3A).is_none()); // caps lock on.
        assert!(kb.feed(0x2A).is_none()); // shift down.
        let event = kb.feed(0x1E).unwrap();
        assert_eq!(event.code, KeyCode::from_ascii('a'));
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
        // The unshifted table carries the lowercase letter, so caps lock is
        // what uppercases it — and a digit stays a digit.
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
            leds: 0,
        })));
        assert!(q.push(Event::Key(KeyEvent {
            code: KeyCode::from_ascii('b'),
            released: false,
            shift: false,
            ctrl: false,
            alt: false,
            leds: 0,
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
            leds: 0,
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
                leds: 0,
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
