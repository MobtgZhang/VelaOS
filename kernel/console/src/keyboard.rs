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
use alloc::collections::VecDeque;
use spin::Mutex;

use vela_arch::ps2;
use vela_kernel_input::{status, Event, EventQueue, Keyboard};

/// The status port's auxiliary bit: the waiting byte is a mouse packet.
const MOUSE_DATA: u8 = 1 << 5;

/// The controller's two ports, shared between the console's keyboard poll
/// and the timer tick's mouse drain.
///
/// One lock rather than two readers, because the pair is a *protocol*, not
/// two independent registers: a status read is only meaningful while the
/// byte it describes is still waiting, and two readers racing would see one
/// reader take a byte the other had already decided was its own. The console
/// holds the lock for the length of its poll (it runs as a task and may
/// wait); the tick takes it with `try_lock`, so a tick that lands mid-poll
/// simply skips and lets the next one drain.
static CONTROLLER: Mutex<()> = Mutex::new(());

/// The mouse decoder's state: half a packet, the pointer's position, the
/// buttons down.
///
/// The tick is the only writer — the console discards mouse bytes rather
/// than decoding them, because decoding them is the tick's job — and two
/// CPUs' ticks are the only contention, so a `try_lock` and a skip is the
/// whole story there too.
static MOUSE: Mutex<vela_kernel_input::Mouse> = Mutex::new(vela_kernel_input::Mouse::new());

/// The keyboard decoder's modifier state, shared because the syscall layer
/// is the only caller but a handler may run on any CPU.
static KEYBOARD: Mutex<Keyboard> = Mutex::new(Keyboard::new());

/// The system-wide input queue.
///
/// Every hardware source — the PS/2 controller below and the USB HID driver —
/// publishes the *same* `kernel/input` events here. Intuition does not care
/// which device produced a click, and neither does a window: one queue is what
/// makes the two kinds of mouse indistinguishable to the desktop.
static EVENT_QUEUE: Mutex<EventQueue> = Mutex::new(EventQueue::new());

/// Console characters decoded by the timer tick once input capture is on.
///
/// A small bounded queue rather than a direct controller read: after capture
/// starts the tick owns the controller, and `GetChar` must consume the tick's
/// decoded characters instead of racing it for the same bytes.
const CHAR_CAPACITY: usize = 128;
static CHAR_QUEUE: Mutex<VecDeque<u8>> = Mutex::new(VecDeque::new());

/// Whether the boot menu has finished and the tick may capture the keyboard.
///
/// The boot menu polls the controller directly, so capturing before it closes
/// would steal the very keypress that chooses the session. Entry enables this
/// exactly once, after the menu has answered.
static INPUT_CAPTURE: AtomicBool = AtomicBool::new(false);

/// Whether the controller answered its first poll, so a wedged controller is
/// reported once rather than on every call.
static SEEN_CONTROLLER: AtomicBool = AtomicBool::new(false);

/// Poll the keyboard for one ASCII byte.
///
/// Returns `Some(byte)` when the user has pressed a key this call can turn
/// into text — a printable character, Enter (`\r`) or Backspace (`0x08`).
/// Returns `None` when nothing is waiting, which includes scancodes that
/// decode to nothing (modifiers, releases, unknown keys) and bytes the
/// controller has for the *mouse*, which are left for the timer tick's
/// drain because the mouse has its own consumer and a byte the keyboard
/// decoder would misread as a scancode would make the console type garbage
/// when the pointer moved.
pub fn read_char() -> Option<u8> {
    if INPUT_CAPTURE.load(Ordering::Acquire) {
        // The timer tick is now the controller's owner. Returning one decoded
        // character per call preserves the console's original contract while
        // allowing the same keypress to become an IDCMP keyboard event too.
        return CHAR_QUEUE.lock().pop_front();
    }
    let mut produced = None;
    let mut keyboard = KEYBOARD.lock();
    // The controller is shared with the timer tick's mouse drain, which
    // never waits for this poll — but this poll may wait for the tick. A
    // task holding the lock is the ordinary case; a tick holding it is
    // bounded by the tick's own drain.
    let _controller = CONTROLLER.lock();
    // Drain everything waiting, because one keypress can arrive as several
    // bytes (a modifier plus the key, or a two-byte escape sequence), and
    // leaving the tail queued would make the *next* key late by one.
    while output_full() {
        // Mouse packets also arrive on the data port. The controller marks
        // them with the auxiliary bit in the status byte, and the timer
        // tick's drain is the consumer that decodes them — so this poll
        // leaves the packet alone and stops, rather than reading a byte
        // that belongs to someone else.
        // SAFETY: the status port said a byte is waiting on the data port,
        // which is the only condition under which reading it is defined.
        let status_byte = unsafe { ps2::read_status() };
        let scancode = unsafe { ps2::read_data() };
        SEEN_CONTROLLER.store(true, Ordering::Release);
        if status_byte & MOUSE_DATA != 0 {
            break;
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

/// Wait until the controller accepts a byte, bounded.
///
/// The input-full bit means the controller has not taken the previous write
/// yet. A controller that never clears it would spin forever, so the wait is
/// bounded: a few thousand status reads is far more than a real controller
/// needs and far less than forever.
fn can_write() -> bool {
    for _ in 0..10_000 {
        // SAFETY: the status port is always readable.
        let status_byte = unsafe { ps2::read_status() };
        if status_byte & status::INPUT_FULL == 0 {
            return true;
        }
    }
    false
}

/// Wait until the controller has an answer for the host, bounded.
fn answer_ready() -> bool {
    for _ in 0..10_000 {
        if output_full() {
            return true;
        }
    }
    false
}

/// Enable the mouse, at the controller and at the device.
///
/// The i8042 gates its mouse packets on the *device's* "reporting enabled"
/// flag, which only a `0xF4` command addressed to the auxiliary port can
/// set — and the firmware never sends it (OVMF does not touch the PS/2
/// mouse at all), so without this sequence the controller delivers nothing
/// and the pointer never moves however much the user waves the mouse.
///
/// The command byte is read first and *modified* rather than written
/// blind: the firmware chose the keyboard's translation bit, and a blind
/// write that cleared it would change the scancode set the keyboard
/// decoder was calibrated against.
///
/// Called once at boot, before any user program runs. Idempotent: a second
/// `0xF4` to an already-reporting mouse is a no-op.
pub fn enable_mouse() {
    let _controller = CONTROLLER.lock();
    // Drain anything stale so the transaction below reads its own answers
    // and not a byte the firmware left behind.
    while output_full() {
        // SAFETY: the status port said a byte is waiting.
        let _ = unsafe { ps2::read_data() };
    }
    // Read the command byte: `0x20` is "read", and the answer arrives on
    // the data port.
    // SAFETY: `0x20` is the controller's read-command-byte opcode.
    unsafe { ps2::write_command(0x20) };
    if !answer_ready() {
        return; // A controller that never answers is left as the firmware set it.
    }
    // SAFETY: the wait above guarantees a byte is waiting.
    let command_byte = unsafe { ps2::read_data() };
    // Write it back with both ports' interrupt bits on. The kernel polls
    // rather than interrupting, so these bits are not what makes packets
    // arrive — the device's own reporting flag is — but a controller that
    // would deliver the second port's interrupts with them off is not a
    // controller this kernel wants to be surprised by later.
    // SAFETY: `0x60` is "write the command byte".
    unsafe { ps2::write_command(0x60) };
    if !can_write() {
        return;
    }
    // SAFETY: the wait above means the controller took the previous byte.
    unsafe { ps2::write_data(command_byte | 0x03) };
    // `0xF4` to the auxiliary port: "enable data reporting".
    // SAFETY: `0xD4` directs the next data write to the second port.
    unsafe { ps2::write_command(0xD4) };
    if !can_write() {
        return;
    }
    // SAFETY: as above.
    unsafe { ps2::write_data(0xF4) };
    SEEN_CONTROLLER.store(true, Ordering::Release);
}

/// True when the controller has a byte for the host.
fn output_full() -> bool {
    // SAFETY: the status port is always readable.
    let status_byte = unsafe { ps2::read_status() };
    status_byte & status::OUTPUT_FULL != 0
}

/// Drain the controller's mouse packets into the decoder.
///
/// Called from the timer tick, 250 times a second: a packet the hardware
/// delivers now is decoded now, and the pointer is never further behind the
/// mouse than one 4 ms tick. Returns the event the drain's *last* completed
/// packet produced, if any — the packets carry absolute positions, so an
/// earlier one is already out of date by the time a later one exists.
///
/// # Why every lock here is a `try_lock`
///
/// The caller runs in interrupt context. The console's keyboard poll may
/// hold the `CONTROLLER` lock mid-drain, and another CPU's tick may hold
/// the `MOUSE` lock; waiting for either from an interrupt would deadlock
/// the task the machine is running. Busy locks mean "skip": the packets
/// stay in the controller's buffer, the decoder keeps its half-finished
/// packet, and the next tick picks the whole thing up.
pub fn pump_mouse() -> Option<vela_kernel_input::MouseEvent> {
    let mut mouse = MOUSE.try_lock()?;
    let _controller = CONTROLLER.try_lock()?;
    let mut last = None;
    // The drain stops at a keyboard byte: that byte belongs to the console's
    // poll, and reading it here would eat a keystroke. The aux bit tells the
    // two apart before either reader touches the data port.
    while output_full() {
        // SAFETY: the status port said a byte is waiting.
        let status_byte = unsafe { ps2::read_status() };
        if status_byte & MOUSE_DATA == 0 {
            break;
        }
        // SAFETY: as above — a byte is waiting, and it is ours.
        let byte = unsafe { ps2::read_data() };
        SEEN_CONTROLLER.store(true, Ordering::Release);
        if let Some(event) = mouse.feed(byte) {
            last = Some(event);
        }
    }
    last
}

/// Start routing input through the system event queue.
///
/// Called after the boot menu closes. Before this point the menu and the
/// console poll the controller themselves; after it the timer tick is the one
/// reader, and keyboard bytes are fanned out to both the console queue and
/// Intuition's queue.
pub fn set_input_capture(enabled: bool) {
    if enabled {
        CHAR_QUEUE.lock().clear();
    }
    INPUT_CAPTURE.store(enabled, Ordering::Release);
}

/// Publish a hardware event, from interrupt or task context.
///
/// Returns `false` when the queue is busy or full. Busy means the producer
/// runs on the timer tick and a task is briefly inside the queue; dropping
/// that event is preferable to deadlocking the CPU it interrupted. Ordinary
/// pointer movement with no button change is coalesced with the queued tail:
/// a hundred packets describing one sweep should be one current position,
/// not a backlog that delays the click at its end.
pub fn queue_input_event(event: Event) -> bool {
    let Some(mut queue) = EVENT_QUEUE.try_lock() else {
        return false;
    };
    if let Event::Mouse(mouse) = event {
        if mouse.changed == 0 {
            // A full queue is handled below, so this replacement never needs
            // to allocate and cannot make an already-full queue larger.
            if let Some(Event::Mouse(tail)) = queue.back_mut() {
                if tail.changed == 0 {
                    *tail = mouse;
                    return true;
                }
            }
        }
    }
    queue.push(event)
}

/// Take the next input event for Intuition.
///
/// This is a task-context call: `WaitPort` drains the queue immediately before
/// waiting, so dispatch can take Intuition's ordinary locks without ever
/// taking them in interrupt context.
pub fn pop_input_event() -> Option<Event> {
    EVENT_QUEUE.lock().pop()
}

/// Feed one set-1 scancode from a non-PS/2 keyboard.
///
/// USB HID boot reports are translated to set 1 by the USB driver and arrive
/// here. Sharing the console's decoder means shift, caps lock, control and the
/// ASCII queue behave exactly as they do for a PS/2 keyboard; the event is also
/// published to Intuition through the same queue as every other input source.
pub fn queue_scancode(scancode: u8) -> bool {
    let Some(mut keyboard) = KEYBOARD.try_lock() else {
        return false;
    };
    let Some(event) = keyboard.feed(scancode) else {
        return true;
    };
    let queued = queue_input_event(Event::Key(event));
    if !event.released {
        let ascii = if event.code.to_ascii().is_some() {
            ascii_of(event)
        } else if event.code == vela_kernel_input::KeyCode::ENTER {
            b'\r'
        } else if event.code == vela_kernel_input::KeyCode::BACKSPACE {
            0x08
        } else {
            0
        };
        if ascii != 0 {
            if let Some(mut chars) = CHAR_QUEUE.try_lock() {
                if chars.len() < CHAR_CAPACITY {
                    chars.push_back(ascii);
                }
            }
        }
    }
    queued
}

/// Drain both PS/2 ports, publishing mouse and keyboard events.
///
/// This is the tick's input path once capture is enabled. Every completed
/// mouse packet is queued — not merely the last one — because the final
/// position of a drag does not describe the press and release that happened
/// along the way. The last position is still returned for the software cursor.
pub fn pump_input() -> Option<vela_kernel_input::MouseEvent> {
    if !INPUT_CAPTURE.load(Ordering::Acquire) {
        return pump_mouse();
    }
    let mut mouse = MOUSE.try_lock()?;
    let mut keyboard = KEYBOARD.try_lock()?;
    let _controller = CONTROLLER.try_lock()?;
    let mut last = None;
    while output_full() {
        // SAFETY: the status port said a byte is waiting.
        let status_byte = unsafe { ps2::read_status() };
        // SAFETY: as above — a byte is waiting on the data port.
        let byte = unsafe { ps2::read_data() };
        SEEN_CONTROLLER.store(true, Ordering::Release);
        if status_byte & MOUSE_DATA != 0 {
            if let Some(event) = mouse.feed(byte) {
                let _ = queue_input_event(Event::Mouse(event));
                last = Some(event);
            }
            continue;
        }
        let Some(event) = keyboard.feed(byte) else {
            continue;
        };
        let _ = queue_input_event(Event::Key(event));
        if event.released {
            continue;
        }
        let ascii = if event.code.to_ascii().is_some() {
            ascii_of(event)
        } else if event.code == vela_kernel_input::KeyCode::ENTER {
            b'\r'
        } else if event.code == vela_kernel_input::KeyCode::BACKSPACE {
            0x08
        } else {
            0
        };
        if ascii != 0 {
            let mut chars = CHAR_QUEUE.try_lock()?;
            if chars.len() < CHAR_CAPACITY {
                chars.push_back(ascii);
            }
        }
    }
    last
}
