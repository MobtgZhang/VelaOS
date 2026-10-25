//! `guitest` — the acceptance application.
//!
//! # What this program is for
//!
//! The report's acceptance criterion is one sentence: *a user-space program
//! opens a window, moves the mouse over a gadget, receives the IDCMP event as
//! an Exec message on a port it created, replies, and closes cleanly.* Every
//! link in that chain is a separate subsystem, and this program drives the
//! **whole** chain rather than each piece in isolation.
//!
//! So this is not a demo of the window manager. It is a program whose entire
//! job is to fail loudly if any link in that chain is broken. It prints a
//! numbered line per step and reports failure rather than hanging, because a
//! guest program that cannot report is a guest program that hangs.
//!
//! # The shape of the event loop
//!
//! One port, one window, one loop. The port belongs to *this* program rather
//! than to the window, which is the whole reason Intuition's events arrive
//! where a program expects them: it created the port, it reads the port, and
//! it replies on the port. Nothing here is a private side channel that a
//! program using the SDK differently would miss.
//!
//! # Running it
//!
//! This file is the *program*, as a library, so that
//! `tests/acceptance.rs` can call [`run`] and assert on what it saw without
//! going through an entry point. The guest entry point — the one that prints
//! the result to the serial log and powers the machine off — is
//! `src/main.rs`, and it does nothing except call [`run`] and report.
//!
//! On bare metal the kernel starts it after `init`. Under `cargo test` the
//! harness substitutes the `syscall` instruction with a direct call into the
//! real kernel services — the same code the real gate calls, only the
//! transition differs — so `tests/acceptance.rs` drives this exact logic and
//! asserts on its exact result.

#![cfg_attr(target_os = "none", no_std)]

extern crate alloc;

use vela_sys::abi::{
    buttons, gadget_kind, idcmp, mouse_codes, request_codes, window_flags, IntuiMessage, Message,
    OpenWindowArgs, Rectangle,
};
use vela_sys::api;

/// Where the window goes, in screen coordinates.
///
/// Far enough from the origin that the mouse events the test synthesises are
/// unambiguously over the gadget and not over the window's own furniture.
const WINDOW_X: i16 = 40;
const WINDOW_Y: i16 = 40;
const WINDOW_W: u16 = 320;
const WINDOW_H: u16 = 240;

/// The button gadget, in **window-local** coordinates.
///
/// Local, not screen, because that is what `AddGadget` takes and what AmigaOS
/// takes. The distinction is load-bearing: a window moves, and its gadgets move
/// with it — a gadget stored in screen coordinates would stay behind when its
/// window moved, which is the bug this placement is checking.
pub const GADGET_X: i16 = 60;
pub const GADGET_Y: i16 = 100;
pub const GADGET_W: u16 = 80;
pub const GADGET_H: u16 = 24;

/// The toggle gadget, below the button, so that routing by position is
/// exercised rather than assumed: a broken hit test that always returned the
/// first gadget would pass a one-gadget test and fail this one.
pub const TOGGLE_X: i16 = 60;
pub const TOGGLE_Y: i16 = 140;
pub const TOGGLE_W: u16 = 80;
pub const TOGGLE_H: u16 = 24;

/// Where the button lands in screen coordinates, for the test's mouse events.
///
/// Derived rather than written down, so the two can never drift: the mouse
/// works in screen coordinates and the gadget in window-local ones, and a test
/// that computed one from the other could never fail when they disagreed.
pub fn button_screen_pos() -> (i16, i16) {
    (WINDOW_X + GADGET_X, WINDOW_Y + GADGET_Y)
}

/// Where the toggle lands in screen coordinates.
pub fn toggle_screen_pos() -> (i16, i16) {
    (WINDOW_X + TOGGLE_X, WINDOW_Y + TOGGLE_Y)
}

/// The signal mask for this program's port.
///
/// `SIGB_MESSAGE` plus `SIGB_SINGLE` is what makes `WaitPort` return. A port
/// created with only `SIGB_MESSAGE` would queue messages without waking
/// anybody, which is the difference between a working port and a hung program.
pub const SIGB: u32 = 0x4000;

/// The screen's geometry, as VelaOS's QEMU display is configured.
const SCREEN_W: i16 = 1280;
const SCREEN_H: i16 = 800;

/// The screen's title.
const SCREEN_TITLE: &str = "guitest";

/// The window's title.
const WINDOW_TITLE: &str = "guitest";

/// What the program observed.
///
/// Kept as a value rather than only printed, so the hosted test asserts on the
/// *behaviour* and not on the text — and so a mismatch says what the program
/// saw, not merely that its output differed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Outcome {
    /// How many events arrived in total.
    pub events: u32,
    /// How many were gadget events.
    pub gadget_events: u32,
    /// How many were close requests.
    pub close_events: u32,
    /// How many were plain mouse movements.
    pub mouse_moves: u32,
    /// How many button-down events.
    pub button_downs: u32,
    /// How many button-up events.
    pub button_ups: u32,
    /// How many requests were replied to.
    ///
    /// Not the same as `events`: a reply to this program's own reply arrives on
    /// its port and is *not* a request, so replying to that would be an error
    /// and, because the port is its own reply port, would feed the reply back
    /// in forever. [`Self::is_accepted`] requires one reply per request, and
    /// `requests` is the honest denominator.
    pub replies: u32,
    /// How many messages carried a reply port, i.e. how many asked for an
    /// answer.
    pub requests: u32,
}

impl Outcome {
    /// True when the chain worked end to end.
    ///
    /// The specific numbers rather than "something arrived": a program that
    /// received three of four expected events has a bug, and a check that only
    /// looked for traffic would pass it.
    ///
    /// Every *request* is replied to — not every message. See
    /// [`Self::requests`].
    pub const fn is_accepted(&self) -> bool {
        self.gadget_events >= 2
            && self.button_downs >= 2
            && self.button_ups >= 2
            && self.requests > 0
            && self.replies == self.requests
            && self.close_events >= 1
    }
}

/// How many events the loop will take before giving up.
///
/// Bounded, because a program that waits forever on a port that will never be
/// posted to is indistinguishable from a hung kernel, and the difference
/// matters enormously when diagnosing a failure. Generous enough for the test
/// to drive every class it cares about, small enough that a broken delivery
/// path fails in seconds rather than minutes.
pub const MAX_EVENTS: u32 = 64;

/// How many times the loop may come up empty before giving up.
///
/// The loop's own bound is on *events*, but `wait_port` also returns without a
/// message when something other than the port woke the task. Without a second
/// bound on the empty returns, a program that never receives the events it is
/// waiting for spins forever, and a program that spins forever is
/// indistinguishable from a hung kernel — including to the test that is trying
/// to find out.
///
/// Generous enough to be irrelevant on hardware, where the wait *blocks* in
/// the scheduler rather than spinning, and small enough that a broken delivery
/// path fails in seconds rather than minutes.
pub const MAX_EMPTY_WAITS: u32 = 4_000_000;

/// A failure that stops the program.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Failure {
    /// The syscall layer's error code.
    pub code: i32,
    /// Which step failed, as a short label.
    pub step: &'static str,
}

impl From<vela_sys::Error> for Failure {
    fn from(e: vela_sys::Error) -> Self {
        Self {
            code: e.code,
            step: "syscall",
        }
    }
}

/// Run the acceptance program against whatever kernel is underneath.
///
/// Returns what it observed rather than printing and returning, so the hosted
/// test drives the same code the bare-metal `main` does.
pub fn run() -> Result<Outcome, Failure> {
    let mut outcome = Outcome::default();

    // A port to receive events on. `exec.library` is always open, so nothing
    // needs opening before this.
    let port = at("create_port", api::create_port(SIGB))?;

    // Everything else is opened by name, as on AmigaOS. A library that is not
    // there yields a null handle rather than an error, which is what makes
    // `if (lib = OpenLibrary(...))` the AmigaOS idiom — so a null handle here
    // is expected on a kernel that has not implemented it, not a failure.
    api::open_library("intuition.library").ok();
    api::open_library("graphics.library").ok();

    // A window lives on a screen, so the screen comes first.
    let screen = at(
        "open_screen",
        api::open_screen(
            SCREEN_TITLE,
            &Rectangle::new(0, 0, SCREEN_W as u16, SCREEN_H as u16),
        ),
    )?;

    let args = OpenWindowArgs {
        x: WINDOW_X,
        y: WINDOW_Y,
        width: WINDOW_W,
        height: WINDOW_H,
        // Everything, so the test can drive any event class and find them all
        // routed here rather than silently dropped.
        idcmp: idcmp::ALL,
        flags: window_flags::CLOSEGADGET | window_flags::DEPTHGADGET,
    };
    let window = at(
        "open_window",
        api::open_window(screen, WINDOW_TITLE, &args, port),
    )?;

    // Two gadgets, so that event routing by position is exercised.
    let button = at(
        "add_gadget(button)",
        api::add_gadget(
            window,
            gadget_kind::BUTTON,
            &Rectangle::new(GADGET_X, GADGET_Y, GADGET_W, GADGET_H),
            "Press me",
        ),
    )?;
    let toggle = at(
        "add_gadget(toggle)",
        api::add_gadget(
            window,
            gadget_kind::TOGGLE,
            &Rectangle::new(TOGGLE_X, TOGGLE_Y, TOGGLE_W, TOGGLE_H),
            "Toggle me",
        ),
    )?;

    // Draw something, so a screenshot of a successful run shows the program
    // painted rather than merely existing. A pen failure is not fatal: the
    // program still exercises every link in the acceptance chain.
    let pen = api::graphics::alloc_pen(0, api::colors::GADGET);
    if pen.is_valid() {
        // `let _` on each result because a drawing failure must not stop the
        // event chain from being tested — a program that drew nothing and a
        // program that crashed before its first event are different bugs, and
        // conflating them makes diagnosing either harder.
        let _ = api::graphics::fill_rect(window, pen, &Rectangle::new(10, 30, 200, 60));
        let _ = api::graphics::text(window, pen, 14, 50, "guitest: mouse the gadgets");
    }
    // Push the window to the front and show the drawing.
    let _ = api::repaint_window(window, true);

    // The event loop.
    //
    // Two bounds, on purpose. `events` counts what arrived, so it stops a
    // program that receives a flood; `empty_waits` counts the returns with
    // nothing on the port, so it stops a program that receives *nothing* —
    // which is the failure this program exists to detect, and which an
    // events-only bound would wait on forever.
    let mut empty_waits: u32 = 0;
    while outcome.events < MAX_EVENTS {
        let Some(mut msg) = at("wait_port", api::wait_port(port))? else {
            // The wait ended without a message. Not an error — the kernel says
            // so plainly — but there is nothing to do and no way to tell how
            // long it will stay that way, so yielding is the only forward
            // progress available.
            empty_waits += 1;
            if empty_waits >= MAX_EMPTY_WAITS {
                return Err(Failure {
                    code: 0,
                    step: "wait_port: no events arrived",
                });
            }
            let _ = api::yield_now();
            continue;
        };
        // A message resets the empty-wait count: the delivery path is alive, so
        // the program is waiting for the *next* event, not for any event.
        empty_waits = 0;

        outcome.events += 1;
        let event = msg.as_intui();
        classify(&event, &mut outcome);

        // Reply to every message that is a *request*, including the ones the
        // program does not act on. A request left unreplied is a sender waiting
        // forever, and Intuition is a sender like any other.
        //
        // A message with no reply port is not a request: it is either a reply
        // to something this program already answered, or a notification that
        // expects no answer. Replying to one is an error the kernel reports as
        // `NO_REPLY_PORT` — and because this program's port is its own reply
        // port, replying to its own replies feeds them straight back into the
        // queue, so a program that does this never drains its port and fails
        // with an error that points at Exec rather than at the mistake.
        if msg.mn_reply_port != 0 {
            outcome.requests += 1;
            let mut reply = Message::empty();
            outcome.replies += 1;
            at("reply_msg", api::reply_msg(&mut msg, &mut reply))?;
        }

        // The acceptance is complete once the button was pressed, the toggle
        // was pressed, and the window was closed — three events that between
        // them cover routing, both button edges, replies, and teardown.
        if outcome.gadget_events >= 2 && outcome.button_downs >= 2 && outcome.close_events >= 1 {
            break;
        }
    }

    // Close cleanly, back to front. `CloseWindow` frees the gadgets, so they
    // are removed explicitly first — which is what a program does when it
    // wants them gone *before* the window rather than with it. The port goes
    // last, because it is what the events arrive on and the window's teardown
    // purges anything still queued on it.
    at("remove_gadget(toggle)", api::remove_gadget(window, toggle))?;
    at("remove_gadget(button)", api::remove_gadget(window, button))?;
    at("close_window", api::close_window(window))?;
    at("close_screen", api::close_screen(screen))?;
    at("delete_port", api::delete_port(port))?;

    Ok(outcome)
}

/// Label a failing step, so the report says *which* call failed.
fn at<T>(step: &'static str, result: Result<T, vela_sys::Error>) -> Result<T, Failure> {
    result.map_err(|e| Failure { code: e.code, step })
}

/// Fold one event into the running tally.
///
/// # Reading a gadget event
///
/// The kernel codes a gadget event as `GADGET` with `im_Code` set to the
/// *gadget's id*, not to a button code — the id is what lets a program with
/// several gadgets tell them apart. The press/release distinction is therefore
/// not in `im_Code` at all: it is in `im_Button`, which carries the buttons
/// that were down when the event was made.
///
/// Reading the button mask rather than a code is what makes this correct for a
/// click on a gadget, a *press* and a *release* alike — which are three
/// separate messages, and which a program that counted only one of them would
/// report as a missed event.
fn classify(event: &IntuiMessage, outcome: &mut Outcome) {
    let class = event.im_Type;
    if class == IntuiMessage::GADGET {
        outcome.gadget_events += 1;
        // Left button down means a press; left button up means the release.
        // Any other button is neither, and is counted as neither — a
        // right-click on a gadget is a real event but not a press of it.
        if event.im_Button & buttons::LEFT != 0 {
            outcome.button_downs += 1;
        } else {
            outcome.button_ups += 1;
        }
    } else if class == IntuiMessage::MOUSE {
        // A movement with no button down. A movement that arrives with a
        // button down is really a button event that happens to carry
        // coordinates, and counting it as both would double-count every click.
        if event.im_Code == mouse_codes::MOVE && event.im_Button == 0 {
            outcome.mouse_moves += 1;
        }
    } else if class == IntuiMessage::REQUEST && event.im_Code == request_codes::CLOSE {
        outcome.close_events += 1;
    }
}
