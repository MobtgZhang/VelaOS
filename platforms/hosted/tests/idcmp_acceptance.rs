//! The M3/M4 acceptance criterion, run under `cargo test`.
//!
//! # What this file proves
//!
//! The report's acceptance path is a single sentence: a user-space program
//! opens a window, moves the mouse over it, receives the IDCMP event as an
//! Exec message on a port *it* created, replies, and closes cleanly. Every
//! link in that chain is a separate subsystem, and this file drives the whole
//! chain rather than each piece in isolation:
//!
//! ```text
//!   mouse event → Intuition::dispatch_mouse
//!               → Exec::put_msg on the program's port
//!               → the program reads it with GetMsg
//!               → the program replies
//!               → the program closes the window
//!               → the port has no messages left
//! ```
//!
//! # Why the harness is enough for this
//!
//! Every arrow above is a call into the *real* service: the real hit-test, the
//! real `PutMsg`, the real `ReplyMsg`, the real message-port teardown. What
//! the harness substitutes is the `syscall` instruction itself — a hosted
//! "user pointer" is a real host address, so the user-copy validation is
//! genuinely exercised rather than bypassed.
//!
//! The one thing it cannot prove is that the assembly gate reaches these
//! services from ring 3. That is the boot smoke test's job, and the two
//! together cover the path.

use vela_abi_native64::{buttons as abi_buttons, mouse_codes, IntuiMessage, Message};
use vela_hosted::{Kernel, HOSTED_SCREEN_HEIGHT, HOSTED_SCREEN_WIDTH};
use vela_kernel_input::{buttons, MouseEvent};
use vela_kernel_intuition::{idcmp, request_codes, window_flags, ScreenId, WindowId};
use vela_kernel_layers::{LayerId, LayerKind};
use vela_kernel_syscall::copy::UserRect;
use vela_kernel_syscall::intuition_handlers::OpenWindowArgs;
use vela_kernel_syscall::numbers::{self as num, errors, SyscallArgs};

/// The window the acceptance program opens.
///
/// Sized and placed so that a gadget at `(100, 140)` is comfortably inside the
/// window's content area and comfortably outside its title bar and its close
/// gadget — so a click at that point unambiguously hits the registered gadget
/// rather than the furniture.
const WIN_X: i16 = 40;
const WIN_Y: i16 = 40;
const WIN_W: u16 = 320;
const WIN_H: u16 = 240;

/// Where the acceptance program's clickable gadget is, **in window-local
/// coordinates** — which is what `AddGadget` takes and what AmigaOS takes.
///
/// The distinction is load-bearing: a window moves, and its gadgets move with
/// it. A gadget stored in screen coordinates would stay behind when its
/// window moved, which is the bug this constant's placement is checking.
const GADGET_X: i16 = 60;
const GADGET_Y: i16 = 100;
const GADGET_W: u16 = 80;
const GADGET_H: u16 = 24;

/// Where the gadget lands in screen coordinates, for the mouse events.
///
/// Derived rather than written down so the two can never drift apart: the
/// mouse works in screen coordinates and the gadget in window-local ones, and
/// a test that computed one from the other is a test that cannot fail when
/// they disagree.
const GADGET_SCREEN_X: i32 = WIN_X as i32 + GADGET_X as i32;
const GADGET_SCREEN_Y: i32 = WIN_Y as i32 + GADGET_Y as i32;

/// A point inside the gadget, in screen coordinates.
fn over_gadget() -> (i32, i32) {
    (
        GADGET_SCREEN_X + GADGET_W as i32 / 2,
        GADGET_SCREEN_Y + GADGET_H as i32 / 2,
    )
}

/// A point inside the gadget but near its left edge, in screen coordinates.
///
/// Near the edge rather than the middle so a click cannot accidentally land on
/// a neighbour if the geometry is off by a few pixels.
fn over_gadget_corner() -> (i32, i32) {
    (GADGET_SCREEN_X + 4, GADGET_SCREEN_Y + 4)
}

/// The signal mask a program uses for its port.
///
/// `SIGB_MESSAGE` plus `SIGB_SINGLE` is what makes `WaitPort` return; a port
/// created with only `SIGB_MESSAGE` would queue messages without waking
/// anybody.
const SIGB: u32 = 0x4000;

/// Everything a window-opening program needs.
struct Program {
    kernel: Kernel,
    task: vela_kernel_exec::task::TaskId,
    screen: ScreenId,
    window: WindowId,
    port: vela_kernel_exec::port::PortId,
    gadget: vela_kernel_intuition::GadgetId,
}

/// Open a screen, a port and a window, and add one button gadget.
///
/// Every call goes through `Kernel::syscall`, so this is the same path a ring
/// 3 program takes.
fn open_program() -> Program {
    let kernel = Kernel::new();
    let task = kernel.exec().create_task();

    // A port the *program* owns. The window is given this port rather than
    // Intuition creating one, because the program needs to read from it.
    let created = kernel.syscall(
        task,
        num::CREATE_MSG_PORT,
        SyscallArgs::new(SIGB as u64, SIGB as u64, 0, 0, 0, 0),
    );
    assert!(created.is_ok(), "CreateMsgPort failed: {}", created.error);
    let port = vela_kernel_exec::port::PortId(created.value as u32);

    // The screen. A hosted window manager always has one; opening it here is
    // what a real program's `OpenScreen` would do.
    let title = b"Vela Acceptance\0";
    let screen_result = kernel.syscall(
        task,
        num::INTUITION_OPEN_SCREEN,
        SyscallArgs::new(
            title.as_ptr() as u64,
            0,
            0,
            HOSTED_SCREEN_WIDTH as u64,
            HOSTED_SCREEN_HEIGHT as u64,
            0,
        ),
    );
    assert!(
        screen_result.is_ok(),
        "OpenScreen failed: {}",
        screen_result.error
    );
    let screen = ScreenId(screen_result.value as u32);

    let window_title = b"guitest\0";
    let spec = OpenWindowArgs {
        x: WIN_X,
        y: WIN_Y,
        width: WIN_W,
        height: WIN_H,
        idcmp: idcmp::MOUSEBUTTONS | idcmp::GADGET | idcmp::REQUEST,
        flags: window_flags::CLOSEGADGET,
    };
    let window_result = kernel.syscall(
        task,
        num::INTUITION_OPEN_WINDOW,
        SyscallArgs::new(
            screen.0 as u64,
            window_title.as_ptr() as u64,
            &spec as *const OpenWindowArgs as u64,
            port.0 as u64,
            0,
            0,
        ),
    );
    assert!(
        window_result.is_ok(),
        "OpenWindow failed: {}",
        window_result.error
    );
    let window = WindowId(window_result.value as u32);

    let gadget_label = b"Press me\0";
    let gadget_rect = UserRect::new(GADGET_X, GADGET_Y, GADGET_W, GADGET_H);
    let gadget_result = kernel.syscall(
        task,
        num::INTUITION_ADD_GADGET,
        SyscallArgs::new(
            window.0 as u64,
            0, // GadgetKind::Button
            &gadget_rect as *const UserRect as u64,
            gadget_label.as_ptr() as u64,
            0,
            0,
        ),
    );
    assert!(
        gadget_result.is_ok(),
        "AddGadget failed: {}",
        gadget_result.error
    );

    Program {
        kernel,
        task,
        screen,
        window,
        port,
        gadget: vela_kernel_intuition::GadgetId(gadget_result.value as u32),
    }
}

impl Program {
    /// Take one message off the program's port, as the program would.
    fn get_msg(&self) -> Option<Message> {
        let mut message = Message::empty();
        let result = self.kernel.syscall(
            self.task,
            num::GET_MSG,
            SyscallArgs::new(
                self.port.0 as u64,
                &mut message as *mut Message as u64,
                0,
                0,
                0,
                0,
            ),
        );
        assert!(result.is_ok(), "GetMsg failed: {}", result.error);
        Some(message)
    }

    /// True when the port has nothing waiting.
    fn port_is_empty(&self) -> bool {
        self.kernel.exec().ports_queue_len(self.port) == 0
    }

    /// Drain every message the port holds.
    fn drain(&self) -> Vec<IntuiMessage> {
        let mut out = Vec::new();
        while self.kernel.exec().ports_queue_len(self.port) > 0 && out.len() <= 64 {
            match self.get_msg() {
                Some(m) => out.push(m.as_intui()),
                None => break,
            }
        }
        out
    }
}

// ---------------------------------------------------------------------------
// The acceptance path itself
// ---------------------------------------------------------------------------

#[test]
fn a_program_receives_a_real_mouse_idcmp_as_an_exec_message() {
    // The whole chain, in one test, because the plan's acceptance criterion
    // is the chain and not any single link.
    let p = open_program();

    // The mouse presses on the gadget.
    let (mx, my) = over_gadget();
    let hit = p.kernel.intuition().dispatch_mouse(
        p.kernel.exec(),
        &MouseEvent {
            x: mx,
            y: my,
            buttons: buttons::LEFT,
            changed: buttons::LEFT,
        },
    );
    assert_eq!(hit, Some(p.window), "the click must reach the window");

    // The program reads the message off *its own* port.
    let message = p.get_msg().expect("a message must be waiting");
    assert_eq!(
        message.mn_reply_port, p.port.0 as u64,
        "the message must carry the program's port as its reply port",
    );

    // The message is a gadget event aimed at this window's gadget.
    let intui = message.as_intui();
    assert_eq!(intui.im_Type, IntuiMessage::GADGET);
    assert_eq!(
        intui.im_Code as u32, p.gadget.0,
        "the event must name the gadget under the pointer",
    );
    assert_eq!(
        intui.im_Source, p.window.0 as u64,
        "the event must name the window that produced it",
    );
    // The coordinates are *window-local*, which is what lets a program use
    // them without knowing where its window is.
    assert_eq!(intui.im_MouseX, GADGET_X + GADGET_W as i16 / 2);
    assert_eq!(intui.im_MouseY, GADGET_Y + GADGET_H as i16 / 2);

    // Reply, as the program must.
    let reply = Message::empty();
    let replied = p.kernel.syscall(
        p.task,
        num::REPLY_MSG,
        SyscallArgs::new(
            &message as *const Message as u64,
            &reply as *const Message as u64,
            0,
            0,
            0,
            0,
        ),
    );
    assert!(replied.is_ok(), "ReplyMsg failed: {}", replied.error);
}

#[test]
fn the_reply_goes_back_through_the_reply_port_the_sender_chose() {
    // The half of the path that proves the reply is a real message on a real
    // port rather than a write into a struct the sender still holds.
    let p = open_program();
    let other = p.kernel.exec().create_task();
    // The *sender* holds a port. It sends a request to the program's port and
    // puts its own port in the message as the reply port.
    let sender_port = p.kernel.exec().create_port(other, SIGB, SIGB, 0);

    let intui = IntuiMessage::gadget(p.gadget.0 as u16, p.window.0 as u64);
    let mut request = Message::with_intui(intui);
    request.mn_reply_port = sender_port.0 as u64;
    p.kernel.exec().put_msg(p.port, request).unwrap();

    // The program reads the request and replies.
    let got = p
        .get_msg()
        .expect("the request must arrive on the program's port");
    assert_eq!(
        got.mn_reply_port, sender_port.0 as u64,
        "the request must carry the sender's port as its reply port",
    );
    let reply = Message::empty();
    let replied = p.kernel.syscall(
        p.task,
        num::REPLY_MSG,
        SyscallArgs::new(
            &got as *const Message as u64,
            &reply as *const Message as u64,
            0,
            0,
            0,
            0,
        ),
    );
    assert!(replied.is_ok(), "ReplyMsg failed: {}", replied.error);

    // The reply landed on the *sender's* port, which is the whole point: the
    // program never named that port, it only handed it back.
    assert_eq!(
        p.kernel.exec().ports_queue_len(sender_port),
        1,
        "the reply must arrive on the sender's port",
    );
    assert!(
        p.port_is_empty(),
        "and must not come back to the program that sent the reply",
    );
    let back = p.kernel.exec().get_msg(sender_port).unwrap();
    assert_eq!(
        back.mn_reply_port, 0,
        "a reply's own reply port is cleared: nothing replies to a reply",
    );
}

#[test]
fn a_click_outside_every_window_reaches_nobody() {
    // The negative case, because a hit-test that always returned the topmost
    // window would pass every positive test.
    let p = open_program();
    let hit = p.kernel.intuition().dispatch_mouse(
        p.kernel.exec(),
        &MouseEvent {
            x: 0,
            y: 0,
            buttons: buttons::LEFT,
            changed: buttons::LEFT,
        },
    );
    // (0, 0) is on the screen but not on the window.
    assert_ne!(hit, Some(p.window));
    assert!(p.port_is_empty(), "no event must be delivered");
}

#[test]
fn only_a_window_the_program_owns_can_be_drawn_into() {
    // The ownership rule is what stops one program scribbling over another's
    // window. It is checked here because `paint_window` enforces it and a
    // handler that forgot the check would be invisible to every other test.
    let p = open_program();
    let intruder = p.kernel.exec().create_task();

    let rect = UserRect {
        x: 0,
        y: 0,
        w: 16,
        h: 16,
    };
    let pen = alloc_pen(&p, 0x00FF_0000);
    let result = p.kernel.syscall(
        intruder,
        num::GRAPHICS_FILL_RECT,
        SyscallArgs::new(
            p.window.0 as u64,
            pen as u64,
            &rect as *const UserRect as u64,
            0,
            0,
            0,
        ),
    );
    assert_eq!(
        result.error,
        errors::NOT_OWNER,
        "another task must not be able to draw into this window",
    );

    // The owner can.
    let own = p.kernel.syscall(
        p.task,
        num::GRAPHICS_FILL_RECT,
        SyscallArgs::new(
            p.window.0 as u64,
            pen as u64,
            &rect as *const UserRect as u64,
            0,
            0,
            0,
        ),
    );
    assert!(own.is_ok(), "the owner must be able to draw: {}", own.error);
}

#[test]
fn closing_the_window_leaves_no_messages_naming_it() {
    // Task exit cleanup: a window closed under a program that never drained
    // its port must not leave messages that name a window that no longer
    // exists.
    let p = open_program();
    // Produce a few events so the port has something in it.
    for i in 0..3 {
        p.kernel.intuition().dispatch_mouse(
            p.kernel.exec(),
            &MouseEvent {
                x: GADGET_SCREEN_X + i,
                y: GADGET_SCREEN_Y + i,
                buttons: buttons::LEFT,
                changed: buttons::LEFT,
            },
        );
    }
    assert!(!p.port_is_empty(), "the events must be waiting first");

    let closed = p.kernel.syscall(
        p.task,
        num::INTUITION_CLOSE_WINDOW,
        SyscallArgs::new(p.window.0 as u64, 0, 0, 0, 0, 0),
    );
    assert!(closed.is_ok(), "CloseWindow failed: {}", closed.error);

    // The window is gone.
    assert!(p.kernel.intuition().window(p.window).is_none());
    assert!(
        p.port_is_empty(),
        "closing a window must discard its pending events, not leave them \
         naming a window that no longer exists",
    );
}

#[test]
fn closing_a_window_someone_else_owns_is_refused() {
    let p = open_program();
    let intruder = p.kernel.exec().create_task();
    let result = p.kernel.syscall(
        intruder,
        num::INTUITION_CLOSE_WINDOW,
        SyscallArgs::new(p.window.0 as u64, 0, 0, 0, 0, 0),
    );
    assert_eq!(result.error, errors::NOT_OWNER);
    assert!(
        p.kernel.intuition().window(p.window).is_some(),
        "the window must survive a refused close",
    );
}

#[test]
fn closing_a_screen_closes_every_window_on_it() {
    let p = open_program();
    let closed = p.kernel.syscall(
        p.task,
        num::INTUITION_CLOSE_SCREEN,
        SyscallArgs::new(p.screen.0 as u64, 0, 0, 0, 0, 0),
    );
    assert!(closed.is_ok(), "CloseScreen failed: {}", closed.error);
    assert!(
        closed.value >= 1,
        "CloseScreen must report how many windows it closed",
    );
    assert!(p.kernel.intuition().window(p.window).is_none());
}

#[test]
fn the_window_appears_on_the_layer_stack_above_the_screen() {
    let p = open_program();
    let record = p.kernel.intuition().window(p.window).unwrap();
    let stack = p.kernel.intuition().layers();
    let order = stack.order();

    let screen_layer = order
        .iter()
        .copied()
        .find(|id| stack.layer(*id).map(|l| l.kind) == Some(LayerKind::Screen));
    assert!(screen_layer.is_some(), "a screen layer must exist");
    assert!(
        order.contains(&record.layer),
        "the window's layer must be on the stack",
    );
    assert!(
        position(&order, record.layer) > position(&order, screen_layer.unwrap()),
        "the window must be above the screen",
    );
    assert_eq!(
        stack.layer(record.layer).unwrap().kind,
        LayerKind::Window,
        "a window's layer is a Window, not a Screen",
    );
}

#[test]
fn a_window_paints_its_title_bar_into_its_layer() {
    // The window's furniture must be real pixels, not a description of
    // pixels. This is what makes the QEMU screendump comparison meaningful.
    let p = open_program();
    let repainted = p.kernel.syscall(
        p.task,
        num::INTUITION_REPAINT,
        SyscallArgs::new(p.window.0 as u64, 0, 0, 0, 0, 0),
    );
    assert!(repainted.is_ok(), "repaint failed: {}", repainted.error);

    let record = p.kernel.intuition().window(p.window).unwrap();
    let layer = p.kernel.intuition().layers().layer(record.layer).unwrap();
    let buffer = layer.buffer.as_ref().expect("a window layer has a buffer");

    // The border and title bar are drawn, so the top-left corner is not the
    // same as the middle of the window, which is the backdrop.
    let corner = buffer[0];
    let middle = buffer[(WIN_H as usize / 2) * (WIN_W as usize) + (WIN_W as usize / 2)];
    assert_ne!(
        corner, middle,
        "the window's furniture must be drawn into its layer",
    );
}

#[test]
fn drawing_through_a_bad_user_rectangle_pointer_is_a_clean_error() {
    // The plan requires this case explicitly: a bad user pointer yields an
    // error, never a kernel fault.
    let p = open_program();
    let pen = alloc_pen(&p, 0x00FF_0000);
    let result = p.kernel.syscall(
        p.task,
        num::GRAPHICS_FILL_RECT,
        SyscallArgs::new(
            p.window.0 as u64,
            pen as u64,
            0xFFFF_8000_0000_0000,
            0,
            0,
            0,
        ),
    );
    assert_eq!(result.error, errors::BAD_POINTER);
}

#[test]
fn a_button_gadget_reports_held_while_the_button_is_down() {
    // A plain button is *held*: selected tracks the buttons being down, so
    // the release clears it. A gadget that toggled on both the press and the
    // release would look correct on the first click and wrong on every one
    // after it, so the pair is what the test drives.
    let p = open_program();
    let (mx, my) = over_gadget_corner();

    p.kernel.intuition().dispatch_mouse(
        p.kernel.exec(),
        &MouseEvent {
            x: mx,
            y: my,
            buttons: buttons::LEFT,
            changed: buttons::LEFT,
        },
    );
    assert!(
        gadget_selected(&p, p.gadget),
        "a button is held while the mouse button is down",
    );

    p.kernel.intuition().dispatch_mouse(
        p.kernel.exec(),
        &MouseEvent {
            x: mx,
            y: my,
            buttons: 0,
            changed: buttons::LEFT,
        },
    );
    assert!(
        !gadget_selected(&p, p.gadget),
        "and released when it comes up",
    );

    // Both events were delivered, in order.
    let messages = p.drain();
    assert_eq!(messages.len(), 2, "one press and one release, two messages");
    for m in &messages {
        assert_eq!(m.im_Type, IntuiMessage::GADGET);
        assert_eq!(m.im_Code as u32, p.gadget.0);
    }
    assert_eq!(
        messages[0].im_Button,
        abi_buttons::LEFT,
        "the press must report the button down",
    );
    assert_eq!(
        messages[1].im_Button, 0,
        "the release must report no buttons down",
    );
}

#[test]
fn a_mouse_move_delivers_movement_not_a_click() {
    // A program that wants clicks without tracking must not be woken on every
    // mouse packet — that is why movement and button changes are separate
    // classes.
    let p = open_program();
    let record = p.kernel.intuition().window(p.window).unwrap();
    // Narrow the mask to buttons only.
    p.kernel.syscall(
        p.task,
        num::INTUITION_SET_ATTRS,
        SyscallArgs::new(
            p.window.0 as u64,
            record.flags as u64,
            (idcmp::MOUSEBUTTONS | idcmp::GADGET | idcmp::REQUEST) as u64,
            0,
            0,
            0,
        ),
    );

    // A move over empty space in the window: changed == 0.
    p.kernel.intuition().dispatch_mouse(
        p.kernel.exec(),
        &MouseEvent {
            x: 200,
            y: 200,
            buttons: 0,
            changed: 0,
        },
    );
    assert!(
        p.port_is_empty(),
        "a window that wants no movement must not be woken by a move",
    );

    // Now ask for movement as well.
    p.kernel.syscall(
        p.task,
        num::INTUITION_SET_ATTRS,
        SyscallArgs::new(
            p.window.0 as u64,
            record.flags as u64,
            (idcmp::MOUSE | idcmp::GADGET | idcmp::REQUEST) as u64,
            0,
            0,
            0,
        ),
    );
    p.kernel.intuition().dispatch_mouse(
        p.kernel.exec(),
        &MouseEvent {
            x: 201,
            y: 200,
            buttons: 0,
            changed: 0,
        },
    );
    let messages = p.drain();
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].im_Type, IntuiMessage::MOUSE);
    assert_eq!(messages[0].im_Code, mouse_codes::MOVE);
}

#[test]
fn a_toggle_gadget_latches_on_the_press_and_stays_latched() {
    // A toggle *is* supposed to latch, so this is the gadget whose state
    // changes and stays changed.
    let kernel = Kernel::new();
    let task = kernel.exec().create_task();
    let port = kernel
        .syscall(
            task,
            num::CREATE_MSG_PORT,
            SyscallArgs::new(SIGB as u64, SIGB as u64, 0, 0, 0, 0),
        )
        .value as u32;
    let screen = open_screen(&kernel, task);
    let spec = OpenWindowArgs {
        x: 0,
        y: 0,
        width: 320,
        height: 240,
        idcmp: idcmp::GADGET,
        flags: 0,
    };
    let window = kernel
        .syscall(
            task,
            num::INTUITION_OPEN_WINDOW,
            SyscallArgs::new(
                screen.0 as u64,
                c"t".as_ptr() as u64,
                &spec as *const OpenWindowArgs as u64,
                port as u64,
                0,
                0,
            ),
        )
        .value as u32;
    let toggle_rect = UserRect::new(40, 60, 80, 24);
    let gadget = kernel
        .syscall(
            task,
            num::INTUITION_ADD_GADGET,
            SyscallArgs::new(
                window as u64,
                2, // GadgetKind::Toggle
                &toggle_rect as *const UserRect as u64,
                c"x".as_ptr() as u64,
                0,
                0,
            ),
        )
        .value as u32;
    let id = vela_kernel_intuition::GadgetId(gadget);

    let event = |buttons_now: u8, changed: u8| MouseEvent {
        x: 60,
        y: 70,
        buttons: buttons_now,
        changed,
    };
    let selected = |kernel: &Kernel| {
        kernel
            .intuition()
            .window(WindowId(window))
            .unwrap()
            .gadgets
            .iter()
            .find(|g| g.id == id)
            .unwrap()
            .selected
    };

    kernel
        .intuition()
        .dispatch_mouse(kernel.exec(), &event(buttons::LEFT, buttons::LEFT));
    assert!(selected(&kernel), "a toggle latches on the press");

    kernel
        .intuition()
        .dispatch_mouse(kernel.exec(), &event(0, buttons::LEFT));
    assert!(selected(&kernel), "and stays latched after the release",);

    // A second press turns it off.
    kernel
        .intuition()
        .dispatch_mouse(kernel.exec(), &event(buttons::LEFT, buttons::LEFT));
    assert!(!selected(&kernel), "a second press turns it off");
}

#[test]
fn a_window_that_wants_no_idcmp_gets_no_messages() {
    let p = open_program();
    let record = p.kernel.intuition().window(p.window).unwrap();
    // Narrow the mask to nothing.
    let attrs = p.kernel.syscall(
        p.task,
        num::INTUITION_SET_ATTRS,
        SyscallArgs::new(p.window.0 as u64, record.flags as u64, 0, 0, 0, 0),
    );
    assert!(attrs.is_ok(), "SetWindowAttrs failed: {}", attrs.error);

    let (mx, my) = over_gadget_corner();
    p.kernel.intuition().dispatch_mouse(
        p.kernel.exec(),
        &MouseEvent {
            x: mx,
            y: my,
            buttons: buttons::LEFT,
            changed: buttons::LEFT,
        },
    );
    assert!(
        p.port_is_empty(),
        "a window that wants nothing gets nothing",
    );
}

#[test]
fn the_close_gadget_delivers_a_request_rather_than_a_gadget_event() {
    // The close gadget is furniture, not a registered gadget, so what arrives
    // is an Intuition *request* — and the program is the one that decides to
    // act on it. The kernel does not close the window behind its back.
    let p = open_program();
    let record = p.kernel.intuition().window(p.window).unwrap();
    let close = record.close_gadget_rect().expect("a close gadget");

    p.kernel.intuition().dispatch_mouse(
        p.kernel.exec(),
        &MouseEvent {
            x: close.x + 1,
            y: close.y + 1,
            buttons: buttons::LEFT,
            changed: buttons::LEFT,
        },
    );

    assert!(
        p.kernel.intuition().window(p.window).is_some(),
        "the kernel must not close a window on its own",
    );
    let messages = p.drain();
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].im_Type, IntuiMessage::REQUEST);
    assert_eq!(messages[0].im_Code, request_codes::CLOSE);
}

#[test]
fn the_topmost_window_gets_the_click_not_the_one_behind_it() {
    let kernel = Kernel::new();
    let task = kernel.exec().create_task();
    let screen = open_screen(&kernel, task);
    let back = open_window(&kernel, task, screen, 10, 10, 200, 150, idcmp::MOUSEBUTTONS);
    let front = open_window(&kernel, task, screen, 60, 60, 200, 150, idcmp::MOUSEBUTTONS);

    // A point inside both windows.
    let hit = kernel.intuition().dispatch_mouse(
        kernel.exec(),
        &MouseEvent {
            x: 100,
            y: 100,
            buttons: buttons::LEFT,
            changed: buttons::LEFT,
        },
    );
    assert_eq!(
        hit,
        Some(front),
        "the frontmost window must get the click, not the one behind it",
    );

    // And the window behind it must have received nothing, even though the
    // point is inside it. One click, one recipient — delivering to both would
    // let a program act on an event it never received.
    let back_port = kernel.intuition().window(back).unwrap().port;
    assert_eq!(
        kernel.exec().ports_queue_len(back_port),
        0,
        "the window behind must not also receive the click",
    );
    let front_port = kernel.intuition().window(front).unwrap().port;
    assert_eq!(
        kernel.exec().ports_queue_len(front_port),
        1,
        "the frontmost window must receive exactly one message",
    );
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn alloc_pen(p: &Program, color: u32) -> u16 {
    let result = p.kernel.syscall(
        p.task,
        num::GRAPHICS_ALLOC_PEN,
        SyscallArgs::new(0, color as u64, 0, 0, 0, 0),
    );
    assert!(result.is_ok(), "AllocPen failed: {}", result.error);
    result.value as u16
}

fn gadget_selected(p: &Program, gadget: vela_kernel_intuition::GadgetId) -> bool {
    p.kernel
        .intuition()
        .window(p.window)
        .unwrap()
        .gadgets
        .iter()
        .find(|g| g.id == gadget)
        .unwrap()
        .selected
}

fn position(order: &[LayerId], id: LayerId) -> usize {
    order.iter().position(|l| *l == id).unwrap_or(usize::MAX)
}

fn open_screen(kernel: &Kernel, task: vela_kernel_exec::task::TaskId) -> ScreenId {
    let result = kernel.syscall(
        task,
        num::INTUITION_OPEN_SCREEN,
        SyscallArgs::new(
            c"s".as_ptr() as u64,
            0,
            0,
            HOSTED_SCREEN_WIDTH as u64,
            HOSTED_SCREEN_HEIGHT as u64,
            0,
        ),
    );
    assert!(result.is_ok(), "OpenScreen failed: {}", result.error);
    ScreenId(result.value as u32)
}

#[allow(clippy::too_many_arguments)]
fn open_window(
    kernel: &Kernel,
    task: vela_kernel_exec::task::TaskId,
    screen: ScreenId,
    x: i16,
    y: i16,
    w: u16,
    h: u16,
    idcmp_mask: u16,
) -> WindowId {
    let port = kernel
        .syscall(
            task,
            num::CREATE_MSG_PORT,
            SyscallArgs::new(SIGB as u64, SIGB as u64, 0, 0, 0, 0),
        )
        .value as u32;
    let spec = OpenWindowArgs {
        x,
        y,
        width: w,
        height: h,
        idcmp: idcmp_mask,
        flags: 0,
    };
    let result = kernel.syscall(
        task,
        num::INTUITION_OPEN_WINDOW,
        SyscallArgs::new(
            screen.0 as u64,
            c"w".as_ptr() as u64,
            &spec as *const OpenWindowArgs as u64,
            port as u64,
            0,
            0,
        ),
    );
    assert!(result.is_ok(), "OpenWindow failed: {}", result.error);
    WindowId(result.value as u32)
}
// ---------------------------------------------------------------------------
// The whole chain, from a PS/2 byte stream
// ---------------------------------------------------------------------------

/// The PS/2 mouse's three-byte packet, in the order the controller sends it.
///
/// The first byte carries the sign bits and the "always one" bit; the second
/// and third are the x and y deltas. The decoder is the *real* one from
/// `kernel/input`, so a test that feeds these bytes exercises the same
/// arithmetic the hardware path does — including the sign extension, which is
/// where the classic bug lives.
///
/// # The bit layout VelaOS's decoder uses
///
/// Bit 3 is the "always one" bit. Bit 0 is the x delta's sign bit and bit 4
/// is the y delta's sign bit. The button state is read from the low three
/// bits — so bit 0 is *also* the left button, and a packet with a negative x
/// delta reads as a left-button press. That is a property of the decoder in
/// `kernel/input` rather than of this test, and it is why the acceptance path
/// below walks the pointer with *positive* deltas: a negative x delta would
/// arrive as a button event rather than a movement.
///
/// A negative delta is encoded the way the hardware encodes it: the byte
/// carries the low eight bits of the value, and the sign bit lives in the
/// first byte. The value is a *nine-bit* two's complement number, so `-1` is
/// the byte `0xFF` — which is exactly the case the sign extension has to get
/// right.
fn ps2_packet(dx: i32, dy: i32) -> [u8; 3] {
    let mut first = 0x08u8;
    if dx < 0 {
        first |= 0x01;
    }
    if dy < 0 {
        first |= 0x10;
    }
    let byte = |value: i32| value as u8;
    [first, byte(dx), byte(dy)]
}

/// The acceptance criterion, end to end, from raw PS/2 bytes.
///
/// This is the gate 7 test. Everything below happens in one function, in
/// order, and nothing reaches around a boundary:
///
/// ```text
///   PS/2 bytes → Mouse::feed → MouseEvent
///              → Intuition::dispatch_mouse  (the kernel's own hit test)
///              → Exec::put_msg on the *program's* port
///              → the program's WaitPort returns the message
///              → the program calls ReplyMsg
///              → the reply arrives at the *sender's* port
///              → the window closes cleanly
/// ```
///
/// The sender is a *second task* with its own port, so the reply is a real
/// message crossing a real task boundary rather than a struct the sender
/// still holds.
#[test]
fn a_ps2_packet_becomes_an_idcmp_message_the_program_replies_to_and_the_window_closes() {
    let kernel = Kernel::new();
    let program_task = kernel.exec().create_task();
    let sender_task = kernel.exec().create_task();

    // The program's own port, created through the syscall boundary.
    let port = vela_kernel_exec::port::PortId(
        kernel
            .syscall(
                program_task,
                num::CREATE_MSG_PORT,
                SyscallArgs::new(SIGB as u64, SIGB as u64, 0, 0, 0, 0),
            )
            .value as u32,
    );

    // The sender's port: where the reply will arrive.
    let sender_port = kernel.exec().create_port(sender_task, SIGB, SIGB, 0);

    // A screen and a window, both through syscalls.
    let screen = open_screen(&kernel, program_task);
    let spec = OpenWindowArgs {
        x: WIN_X,
        y: WIN_Y,
        width: WIN_W,
        height: WIN_H,
        idcmp: idcmp::MOUSE,
        flags: window_flags::CLOSEGADGET,
    };
    let window = WindowId(
        kernel
            .syscall(
                program_task,
                num::INTUITION_OPEN_WINDOW,
                SyscallArgs::new(
                    screen.0 as u64,
                    c"acceptance".as_ptr() as u64,
                    &spec as *const OpenWindowArgs as u64,
                    port.0 as u64,
                    0,
                    0,
                ),
            )
            .value as u32,
    );

    // The pointer starts at the origin and is walked onto the gadget with
    // real PS/2 packets, so the decoder's arithmetic is on the critical path.
    let (target_x, target_y) = over_gadget();
    let mut mouse = vela_kernel_input::Mouse::new();
    mouse.set_bounds(
        0,
        0,
        HOSTED_SCREEN_WIDTH as i32,
        HOSTED_SCREEN_HEIGHT as i32,
    );

    // Move the pointer onto the gadget: one packet per step.
    let mut delivered = Vec::new();
    while (mouse.position().0, mouse.position().1) != (target_x, target_y) {
        let (x, y) = mouse.position();
        let dx = (target_x - x).clamp(0, 64);
        let dy = (target_y - y).clamp(0, 64);
        for byte in ps2_packet(dx, dy) {
            if let Some(event) = mouse.feed(byte) {
                let hit = kernel.intuition().dispatch_mouse(kernel.exec(), &event);
                delivered.push((event, hit));
            }
        }
    }

    // The pointer really is over the gadget now.
    assert_eq!(mouse.position(), (target_x, target_y));
    assert!(
        delivered.iter().any(|(_, hit)| *hit == Some(window)),
        "the window under the pointer must have received the movement",
    );

    // The program waits on its own port and receives the movement.
    //
    // `WaitPort` is the syscall a program uses, and it returns the message
    // *into the program's buffer* — so this is the whole "IDCMP arrives as an
    // Exec message" claim, checked at the boundary rather than inside the
    // kernel.
    let mut message = Message::empty();
    let waited = kernel.syscall(
        program_task,
        num::WAIT_PORT,
        SyscallArgs::new(
            port.0 as u64,
            &mut message as *mut Message as u64,
            0,
            0,
            0,
            0,
        ),
    );
    assert!(waited.is_ok(), "WaitPort failed: {}", waited.error);
    assert_eq!(waited.value, 0, "WaitPort must return the message");

    let intui = message.as_intui();
    assert_eq!(intui.im_Type, IntuiMessage::MOUSE);
    assert_eq!(intui.im_Code, mouse_codes::MOVE);
    assert_eq!(
        intui.im_Source, window.0 as u64,
        "the event must name the window that produced it",
    );

    // Intuition sets the window's port as the reply port, so a program that
    // replies reaches a port rather than failing with `NO_REPLY_PORT`.
    assert_eq!(message.mn_reply_port, port.0 as u64);

    // The walk onto the gadget produced a *series* of movements, and only
    // one of them has been taken. The rest are drained so that the request
    // below is unambiguously the next message on the port — which is what
    // makes the reply-port assertion below meaningful rather than a race
    // with the tail of the movement.
    while kernel.exec().ports_queue_len(port) > 0 {
        kernel.exec().get_msg(port).unwrap();
    }

    // Now the *sender* half: the sender posts a request naming its own port
    // as the reply port, and the program replies to it. The reply must
    // arrive at the sender's port — the program never named that port, it
    // only handed it back.
    let mut request = Message::with_intui(IntuiMessage::gadget(1, window.0 as u64));
    request.mn_reply_port = sender_port.0 as u64;
    kernel
        .exec()
        .put_msg(port, request)
        .expect("the request must be enqueued");

    let mut got = Message::empty();
    let read = kernel.syscall(
        program_task,
        num::GET_MSG,
        SyscallArgs::new(port.0 as u64, &mut got as *mut Message as u64, 0, 0, 0, 0),
    );
    assert!(read.is_ok(), "GetMsg failed: {}", read.error);
    assert_eq!(got.mn_reply_port, sender_port.0 as u64);

    let reply = Message::empty();
    let replied = kernel.syscall(
        program_task,
        num::REPLY_MSG,
        SyscallArgs::new(
            &got as *const Message as u64,
            &reply as *const Message as u64,
            0,
            0,
            0,
            0,
        ),
    );
    assert!(replied.is_ok(), "ReplyMsg failed: {}", replied.error);

    // The reply is on the *sender's* port, and nowhere else.
    assert_eq!(
        kernel.exec().ports_queue_len(sender_port),
        1,
        "the reply must arrive on the sender's port",
    );
    assert_eq!(
        kernel.exec().ports_queue_len(port),
        0,
        "and must not come back to the program that sent it",
    );
    let answer = kernel.exec().get_msg(sender_port).unwrap();
    assert_eq!(
        answer.mn_reply_port, 0,
        "a reply is terminal: it carries no reply port of its own",
    );

    // The window closes cleanly, and closing it leaves nothing behind that
    // names it.
    let closed = kernel.syscall(
        program_task,
        num::INTUITION_CLOSE_WINDOW,
        SyscallArgs::new(window.0 as u64, 0, 0, 0, 0, 0),
    );
    assert!(closed.is_ok(), "CloseWindow failed: {}", closed.error);
    assert!(kernel.intuition().window(window).is_none());
    assert_eq!(
        kernel.exec().ports_queue_len(port),
        0,
        "the program's port must be empty after the window closed",
    );

    // And the port itself can be deleted, with nothing left on it.
    let deleted = kernel.syscall(
        program_task,
        num::DELETE_MSG_PORT,
        SyscallArgs::new(port.0 as u64, 0, 0, 0, 0, 0),
    );
    assert!(deleted.is_ok(), "DeleteMsgPort failed: {}", deleted.error);
}

/// The PS/2 decoder's signed deltas reach Intuition as real coordinates.
///
/// The sign extension is the classic mouse bug: a delta of `0xFF` with the
/// sign bit set is `-1`, not `255`. This test drives the pointer *left* and
/// *up* with negative deltas and checks that the window under the pointer
/// saw the pointer move the right way — which it can only know if the
/// decoder got the sign right.
///
/// The y delta is the clean case, because the y sign bit (bit 4) does not
/// collide with the button bits. The x delta's sign bit is bit 0 — the same
/// bit the left button lives in — so a packet with a negative x delta also
/// reads as a button press. That is a defect in `kernel/input`'s decoder
/// (see the report); this test drives the y axis for the sign check and
/// asserts the x axis separately, so the sign extension is checked without
/// the test depending on the colliding bit.
#[test]
fn a_negative_ps2_delta_moves_the_pointer_the_right_way() {
    let kernel = Kernel::new();
    let task = kernel.exec().create_task();
    let screen = open_screen(&kernel, task);
    let window = open_window(&kernel, task, screen, 200, 200, 200, 150, idcmp::MOUSE);

    // The pointer is walked to a known place first, so the deltas under test
    // are measured from there.
    let mut mouse = vela_kernel_input::Mouse::new();
    mouse.set_bounds(
        0,
        0,
        HOSTED_SCREEN_WIDTH as i32,
        HOSTED_SCREEN_HEIGHT as i32,
    );
    for _ in 0..4 {
        for byte in ps2_packet(64, 64) {
            mouse.feed(byte);
        }
    }
    assert_eq!(mouse.position(), (256, 256));

    // Now a *negative* y delta. The pointer must move to y = 255, not to
    // y = 511 — which is what an unsigned decoder would produce.
    let event = {
        let mut last = None;
        for byte in ps2_packet(0, -1) {
            last = mouse.feed(byte);
        }
        last.expect("a packet must produce an event")
    };
    assert_eq!(
        event.y, 255,
        "a negative y delta must move the pointer up, not down"
    );

    // And a negative x delta must move it left, not right.
    let event = {
        let mut last = None;
        for byte in ps2_packet(-1, 0) {
            last = mouse.feed(byte);
        }
        last.expect("a packet must produce an event")
    };
    assert_eq!(
        event.x, 255,
        "a negative x delta must move the pointer left, not right"
    );

    // The pointer is still inside the window, and Intuition routes the
    // movement to it as an ordinary IDCMP message.
    let hit = kernel.intuition().dispatch_mouse(kernel.exec(), &event);
    assert_eq!(hit, Some(window));
}

/// The reply-port *ownership* rule: a reply transfers the message, not the
/// port.
///
/// The plan calls this out explicitly, and it is the rule that stops two
/// tasks building an unbounded chain of messages. The receiver of a reply
/// still owns its reply port; deleting that port drops any reply still
/// outstanding for it rather than delivering it to freed memory.
#[test]
fn a_reply_transfers_the_message_but_not_the_reply_port() {
    let kernel = Kernel::new();
    let exec = kernel.exec();
    let client = exec.create_task();
    let server = exec.create_task();
    let client_port = exec.create_port(client, SIGB, SIGB, 0);
    let server_port = exec.create_port(server, SIGB, SIGB, 0);

    // The client asks, naming its own port as the reply port.
    let mut request = Message::empty();
    request.mn_reply_port = client_port.0 as u64;
    exec.put_msg(server_port, request).unwrap();

    // The server takes it and replies.
    let got = exec.get_msg(server_port).unwrap();
    exec.reply_msg(&got, Message::empty()).unwrap();

    // The client owns its port still — the reply did not take it away.
    assert_eq!(
        exec.ports.port_owner(client_port),
        Some(client),
        "a reply must not transfer ownership of the reply port",
    );
    assert_eq!(exec.ports_queue_len(client_port), 1);

    // Deleting the client's port hands the outstanding reply back rather
    // than delivering it to freed memory.
    let back = exec.delete_port(client_port).unwrap();
    assert_eq!(back.len(), 1, "the outstanding reply must be handed back");
}

/// The signal mask a port declares is the mask it raises.
///
/// This is the rule that makes a port with a *custom* signal mask work at
/// all: `PutMsg` must raise the port's own `mp_sig`, not a hard-coded
/// "message" bit. A port that raised the wrong bit would queue messages
/// without waking anybody — a hang with no error anywhere.
#[test]
fn a_message_raises_the_ports_own_signal_mask() {
    let kernel = Kernel::new();
    let exec = kernel.exec();
    let owner = exec.create_task();

    // A port with a mask of its own, and nothing else.
    const CUSTOM: u32 = 0x4000;
    let port = exec.create_port(owner, CUSTOM, CUSTOM, 0);
    let signals = exec.task_signals(owner).unwrap();

    exec.put_msg(port, Message::empty()).unwrap();
    assert!(
        signals.has_any(CUSTOM),
        "the port's own mask must be raised by a message",
    );

    // And the bit is cleared when the queue drains, so a wait is not left
    // permanently satisfied.
    exec.get_msg(port).unwrap();
    assert!(
        !signals.has_any(CUSTOM),
        "draining the port must lower the port's own mask",
    );
}
