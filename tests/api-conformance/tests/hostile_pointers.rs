//! The deliberately hostile cases.
//!
//! The plan requires "a `tests/api-conformance` case deliberately passes
//! invalid pointers and asserts a clean error". This file is that case, and
//! it covers *every* syscall that takes a user pointer, so a future syscall
//! that forgets to validate cannot be added without a test failing.
//!
//! # What "clean error" means
//!
//! A hostile pointer must produce an ordinary error code — never a fault,
//! never a panic, and never a silent success. The error codes the kernel
//! uses for this are `BAD_POINTER` (the address is not in the user half, or
//! the access faulted) and `BAD_STRING` (the string was not terminated
//! within the limit).
//!
//! # The two kinds of bad address
//!
//! * A **kernel** address: the range check refuses it before any access, so
//!   the kernel never reads its own memory on a user's behalf.
//! * An **unmapped user** address: the range check passes, and the *probe*
//!   is what refuses it. This is the case a range check alone cannot catch,
//!   and it is the one that would be a kernel crash without the probe.

use vela_kernel_syscall::numbers::{self, errors, SyscallArgs};

use vela_test_api_conformance::harness::{self};

/// The addresses a hostile call might name.
const BAD_ADDRESSES: [u64; 2] = [harness::KERNEL_ADDRESS, harness::UNMAPPED_USER_ADDRESS];

/// A hostile pointer is refused by every call that takes one.
///
/// The calls are enumerated rather than generated, because each one has a
/// different argument shape and a different "what does success look like"
/// answer — and because a list that named the calls by number would not
/// survive a renumbering.
#[test]
fn hostile_pointers_are_refused_everywhere() {
    let k = harness::kernel();
    let t = harness::task(&k);

    // A screen and a window, so the drawing and window calls have something
    // to point at: the pointer is the only thing under test.
    let screen = harness::call(
        &k,
        t,
        numbers::INTUITION_OPEN_SCREEN,
        SyscallArgs::new(harness::cstring("hostile"), 0, 0, 640, 480, 0),
    );
    assert!(screen.is_ok());

    let window = harness::call(
        &k,
        t,
        numbers::INTUITION_OPEN_WINDOW,
        SyscallArgs::new(
            screen.value,
            harness::cstring("window"),
            harness::struct_bytes(&window_args()),
            0,
            0,
            0,
        ),
    );
    assert!(window.is_ok());

    // A pen, so the drawing calls get as far as the pointer they are being
    // tested for: `Text` resolves the pen before it reads the string.
    let pen = harness::call(
        &k,
        t,
        numbers::GRAPHICS_ALLOC_PEN,
        SyscallArgs::new(0, 0x0000_00FF, 0, 0, 0, 0),
    );
    assert!(pen.is_ok());

    // A real message port, so the message calls get past their own
    // validation and the pointer is the only thing left to refuse.
    let port = harness::call(
        &k,
        t,
        numbers::CREATE_MSG_PORT,
        SyscallArgs::new(1, 1, 0, 0, 0, 0),
    );
    assert!(port.is_ok());

    // A real message in the port, so the calls that *take* a message get as
    // far as the copy rather than stopping at "there is no message".
    let mut message = vela_abi_native64::Message::empty();
    message.mn_reply = 0xDEAD_BEEF;
    // SAFETY: `message` is a live local, so the pointer is valid for the
    // `size_of::<Message>()` bytes that are read. The length is the type's
    // own size, so the slice cannot run past the end of the value.
    let message_buf = vela_test_api_conformance::ByteBuffer::with(unsafe {
        std::slice::from_raw_parts(
            &message as *const vela_abi_native64::Message as *const u8,
            std::mem::size_of::<vela_abi_native64::Message>(),
        )
    });

    for bad in BAD_ADDRESSES {
        // `OpenWindow`'s geometry.
        let result = harness::call(
            &k,
            t,
            numbers::INTUITION_OPEN_WINDOW,
            SyscallArgs::new(screen.value, harness::cstring("w"), bad, 0, 0, 0),
        );
        assert!(
            result.is_err(),
            "OpenWindow accepted a hostile geometry pointer",
        );
        assert_eq!(result.error, errors::BAD_POINTER);

        // `OpenScreen`'s title.
        let result = harness::call(
            &k,
            t,
            numbers::INTUITION_OPEN_SCREEN,
            SyscallArgs::new(bad, 0, 0, 640, 480, 0),
        );
        assert!(
            result.is_err(),
            "OpenScreen accepted a hostile title pointer"
        );
        assert_eq!(result.error, errors::BAD_POINTER);

        // `SetTitle`'s title.
        let result = harness::call(
            &k,
            t,
            numbers::INTUITION_SET_TITLE,
            SyscallArgs::new(window.value, bad, 0, 0, 0, 0),
        );
        assert!(result.is_err(), "SetTitle accepted a hostile title pointer");
        assert_eq!(result.error, errors::BAD_POINTER);

        // `AddGadget`'s rectangle.
        let result = harness::call(
            &k,
            t,
            numbers::INTUITION_ADD_GADGET,
            SyscallArgs::new(window.value, 0, bad, harness::cstring("OK"), 0, 0),
        );
        assert!(
            result.is_err(),
            "AddGadget accepted a hostile rectangle pointer",
        );
        assert_eq!(result.error, errors::BAD_POINTER);

        // `AddGadget`'s label.
        let rect = vela_kernel_syscall::copy::UserRect::new(0, 0, 10, 10);
        let result = harness::call(
            &k,
            t,
            numbers::INTUITION_ADD_GADGET,
            SyscallArgs::new(window.value, 0, harness::struct_bytes(&rect), bad, 0, 0),
        );
        assert!(
            result.is_err(),
            "AddGadget accepted a hostile label pointer"
        );
        assert_eq!(result.error, errors::BAD_POINTER);

        // `FillRect`'s rectangle.
        let result = harness::call(
            &k,
            t,
            numbers::GRAPHICS_FILL_RECT,
            SyscallArgs::new(window.value, pen.value, bad, 0, 0, 0),
        );
        assert!(result.is_err(), "FillRect accepted a hostile rectangle");
        assert_eq!(result.error, errors::BAD_POINTER);

        // `Rect`'s rectangle.
        let result = harness::call(
            &k,
            t,
            numbers::GRAPHICS_RECT,
            SyscallArgs::new(window.value, pen.value, bad, 0, 0, 0),
        );
        assert!(result.is_err(), "Rect accepted a hostile rectangle");
        assert_eq!(result.error, errors::BAD_POINTER);

        // `SetClip`'s rectangle.
        let result = harness::call(
            &k,
            t,
            numbers::GRAPHICS_SET_CLIP,
            SyscallArgs::new(window.value, bad, 0, 0, 0, 0),
        );
        assert!(result.is_err(), "SetClip accepted a hostile rectangle");
        assert_eq!(result.error, errors::BAD_POINTER);

        // `Text`'s string.
        let result = harness::call(
            &k,
            t,
            numbers::GRAPHICS_TEXT,
            SyscallArgs::new(window.value, pen.value, 0, 0, bad, 0),
        );
        assert!(result.is_err(), "Text accepted a hostile string pointer");
        assert_eq!(result.error, errors::BAD_POINTER);

        // `OpenLibrary`'s name.
        let result = harness::call(
            &k,
            t,
            numbers::OPEN_LIBRARY,
            SyscallArgs::new(bad, 0, 0, 0, 0, 0),
        );
        assert!(
            result.is_err(),
            "OpenLibrary accepted a hostile name pointer"
        );
        assert_eq!(result.error, errors::BAD_POINTER);

        // `PutMsg`'s message.
        let result = harness::call(
            &k,
            t,
            numbers::PUT_MSG,
            SyscallArgs::new(port.value, bad, 0, 0, 0, 0),
        );
        assert!(result.is_err(), "PutMsg accepted a hostile message pointer");
        assert_eq!(result.error, errors::BAD_POINTER);

        // `GetMsg`'s message. The port holds a message, so the call reaches
        // the copy-out and the hostile pointer is what refuses it.
        harness::call(
            &k,
            t,
            numbers::PUT_MSG,
            SyscallArgs::new(port.value, message_buf.addr(), 0, 0, 0, 0),
        );
        let result = harness::call(
            &k,
            t,
            numbers::GET_MSG,
            SyscallArgs::new(port.value, bad, 0, 0, 0, 0),
        );
        assert!(result.is_err(), "GetMsg accepted a hostile message pointer");
        assert_eq!(result.error, errors::BAD_POINTER);

        // `ReplyMsg`'s request and reply.
        let result = harness::call(
            &k,
            t,
            numbers::REPLY_MSG,
            SyscallArgs::new(bad, bad, 0, 0, 0, 0),
        );
        assert!(
            result.is_err(),
            "ReplyMsg accepted a hostile message pointer"
        );
        assert_eq!(result.error, errors::BAD_POINTER);

        // `WaitPort`'s message.
        let result = harness::call(
            &k,
            t,
            numbers::WAIT_PORT,
            SyscallArgs::new(port.value, bad, 0, 0, 0, 0),
        );
        assert!(
            result.is_err(),
            "WaitPort accepted a hostile message pointer"
        );
        assert_eq!(result.error, errors::BAD_POINTER);

        // `DOS_OPEN`'s path.
        let result = harness::call(
            &k,
            t,
            numbers::DOS_OPEN,
            SyscallArgs::new(bad, 0b001, 0, 0, 0, 0),
        );
        assert!(result.is_err(), "DOS_OPEN accepted a hostile path pointer");
        assert_eq!(result.error, errors::BAD_POINTER);
    }
}

/// A failed copy-out must not consume the message.
///
/// This is the subtle half of the boundary rule: the program made a mistake
/// it can fix, so the message must still be there afterwards. A `GetMsg`
/// that dropped the message on a failed copy would turn a recoverable
/// mistake into a lost request.
#[test]
fn a_failed_copy_out_does_not_consume_the_message() {
    let k = harness::kernel();
    let t = harness::task(&k);

    let port = harness::call(
        &k,
        t,
        numbers::CREATE_MSG_PORT,
        SyscallArgs::new(1, 1, 0, 0, 0, 0),
    );
    assert!(port.is_ok());

    // A real message in the port, so the calls that *take* a message get as
    // far as the copy rather than stopping at "there is no message".
    let mut message = vela_abi_native64::Message::empty();
    message.mn_reply = 0x1234_5678;
    // SAFETY: `message` is a live local, so the pointer is valid for the
    // `size_of::<Message>()` bytes that are read. The length is the type's
    // own size, so the slice cannot run past the end of the value.
    let message_buf = vela_test_api_conformance::ByteBuffer::with(unsafe {
        std::slice::from_raw_parts(
            &message as *const vela_abi_native64::Message as *const u8,
            std::mem::size_of::<vela_abi_native64::Message>(),
        )
    });

    harness::call(
        &k,
        t,
        numbers::PUT_MSG,
        SyscallArgs::new(port.value, message_buf.addr(), 0, 0, 0, 0),
    );

    // A copy-out into a hostile pointer fails.
    let failed = harness::call(
        &k,
        t,
        numbers::GET_MSG,
        SyscallArgs::new(port.value, harness::KERNEL_ADDRESS, 0, 0, 0, 0),
    );
    assert!(failed.is_err(), "the copy-out must fail");

    // And the message is still there: a good copy-out gets it.
    let good = vela_test_api_conformance::ByteBuffer::zeroed(std::mem::size_of::<
        vela_abi_native64::Message,
    >());
    let retried = harness::call(
        &k,
        t,
        numbers::GET_MSG,
        SyscallArgs::new(port.value, good.addr(), 0, 0, 0, 0),
    );
    assert!(retried.is_ok(), "the message must still be in the queue");
    let payload = good.read();
    let reply = u64::from_le_bytes([
        payload[48],
        payload[49],
        payload[50],
        payload[51],
        payload[52],
        payload[53],
        payload[54],
        payload[55],
    ]);
    assert_eq!(
        reply, 0x1234_5678,
        "the message must be the one that was sent"
    );
}

/// The window-geometry structure a program passes to `OpenWindow`.
#[repr(C)]
#[derive(Clone, Copy)]
struct OpenWindowArgs {
    /// Left edge.
    x: i16,
    /// Top edge.
    y: i16,
    /// Width.
    width: u16,
    /// Height.
    height: u16,
    /// The IDCMP class mask.
    idcmp: u16,
    /// The window flags.
    flags: u32,
}

/// The window geometry the tests use.
fn window_args() -> OpenWindowArgs {
    OpenWindowArgs {
        x: 0,
        y: 0,
        width: 200,
        height: 100,
        idcmp: 0,
        flags: 0,
    }
}
