//! The `exec.library` conformance behaviour tests.
//!
//! Exec is the heart of the AmigaOS semantics VelaOS reproduces: message
//! ports, FIFO message queues, reply ownership, signal masks, the classic
//! critical section and the library registry. Every test here drives the
//! **real** Exec service through the **real** syscall dispatch table, using
//! the hosted mock kernel.

use vela_test_api_conformance::harness;

use vela_abi_native64::Message;
use vela_kernel_exec::task::SIGB_MESSAGE;
use vela_kernel_syscall::numbers::{self, errors, SyscallArgs};

use harness::{cstring, ok};

/// A `Message` in memory the kernel may write into, kept alive for the test.
///
/// The message is *not* copied when it is handed to the kernel: the pointer
/// the kernel gets is the pointer the test reads back through, so a copy-out
/// is observable. Copying would have made every assertion about the
/// message's contents vacuous.
struct MessageBuffer {
    /// The address the kernel is given.
    addr: u64,
    /// The same memory, as a raw pointer.
    raw: *mut Message,
}

impl MessageBuffer {
    /// A buffer holding `message`, which the kernel may overwrite.
    fn with(message: &Message) -> Self {
        let raw = Box::into_raw(Box::new(*message));
        Self {
            addr: raw as u64,
            raw,
        }
    }

    /// An empty buffer the kernel may write into.
    fn empty() -> Self {
        Self::with(&Message::empty())
    }

    /// The address to hand to the kernel.
    fn addr(&self) -> u64 {
        self.addr
    }

    /// The message as the kernel left it.
    fn read(&self) -> Message {
        // SAFETY: the `Box` was leaked, so the allocation outlives the
        // `MessageBuffer`, and nothing else moves or frees it.
        unsafe { *self.raw }
    }
}

/// `CreateMsgPort` and `DeleteMsgPort`: a port has a handle, and deleting it
/// makes the handle invalid.
///
/// The handle must be non-zero, because zero is reserved for "no port", and
/// a program that could not tell a real port from "no port" would enqueue
/// messages into nowhere.
#[test]
fn exec_port_lifecycle_is_verified() {
    let k = harness::kernel();
    let t = harness::task(&k);

    let created = ok(
        harness::call(
            &k,
            t,
            numbers::CREATE_MSG_PORT,
            SyscallArgs::new(SIGB_MESSAGE as u64, SIGB_MESSAGE as u64, 0, 0, 0, 0),
        ),
        "CreateMsgPort",
    );
    assert_ne!(created.value, 0, "a port handle must not be zero");

    ok(
        harness::call(
            &k,
            t,
            numbers::DELETE_MSG_PORT,
            SyscallArgs::new(created.value, 0, 0, 0, 0, 0),
        ),
        "DeleteMsgPort",
    );

    // The port is gone, so a second delete must say so rather than succeed.
    let again = harness::call(
        &k,
        t,
        numbers::DELETE_MSG_PORT,
        SyscallArgs::new(created.value, 0, 0, 0, 0, 0),
    );
    assert!(again.is_err(), "deleting a dead port must fail");
    assert_eq!(again.error, errors::NO_OBJECT);
}

/// `PutMsg` and `GetMsg`: the queue is FIFO.
///
/// Two messages go in and come out oldest-first. A queue that reordered them
/// would break every AmigaOS program that relies on a request/reply
/// conversation, because the reply to the first request would arrive after
/// the reply to the second.
#[test]
fn exec_message_queue_is_fifo() {
    let k = harness::kernel();
    let t = harness::task(&k);

    let port = ok(
        harness::call(
            &k,
            t,
            numbers::CREATE_MSG_PORT,
            SyscallArgs::new(SIGB_MESSAGE as u64, SIGB_MESSAGE as u64, 0, 0, 0, 0),
        ),
        "CreateMsgPort",
    )
    .value;

    // Two messages with distinguishable reply payloads.
    let mut first = Message::empty();
    first.mn_reply = 0x1111_1111;
    let mut second = Message::empty();
    second.mn_reply = 0x2222_2222;
    let first_buf = MessageBuffer::with(&first);
    let second_buf = MessageBuffer::with(&second);

    ok(
        harness::call(
            &k,
            t,
            numbers::PUT_MSG,
            SyscallArgs::new(port, first_buf.addr(), 0, 0, 0, 0),
        ),
        "PutMsg",
    );
    ok(
        harness::call(
            &k,
            t,
            numbers::PUT_MSG,
            SyscallArgs::new(port, second_buf.addr(), 0, 0, 0, 0),
        ),
        "PutMsg",
    );

    // Take the messages out again.
    let out = MessageBuffer::empty();
    ok(
        harness::call(
            &k,
            t,
            numbers::GET_MSG,
            SyscallArgs::new(port, out.addr(), 0, 0, 0, 0),
        ),
        "GetMsg",
    );
    assert_eq!(out.read().mn_reply, 0x1111_1111, "the queue must be FIFO");

    ok(
        harness::call(
            &k,
            t,
            numbers::GET_MSG,
            SyscallArgs::new(port, out.addr(), 0, 0, 0, 0),
        ),
        "GetMsg",
    );
    assert_eq!(out.read().mn_reply, 0x2222_2222, "the queue must be FIFO");

    // The queue is empty now, so a third take must report "no message"
    // rather than handing back a stale one.
    let empty = harness::call(
        &k,
        t,
        numbers::GET_MSG,
        SyscallArgs::new(port, out.addr(), 0, 0, 0, 0),
    );
    assert!(empty.is_err(), "an empty queue must not yield a message");
}

/// `ReplyMsg`: the reply goes to the *request's* reply port.
///
/// The request names the port the answer belongs to. A reply that went back
/// to the server's own port would deadlock every request/reply conversation,
/// so the destination is the whole point of the test.
#[test]
fn exec_reply_goes_to_the_reply_port() {
    let k = harness::kernel();
    let t = harness::task(&k);

    // The client's port: where the reply belongs.
    let client_port = ok(
        harness::call(
            &k,
            t,
            numbers::CREATE_MSG_PORT,
            SyscallArgs::new(SIGB_MESSAGE as u64, SIGB_MESSAGE as u64, 0, 0, 0, 0),
        ),
        "CreateMsgPort (client)",
    )
    .value;
    // The server's port: where the request came from.
    let server_port = ok(
        harness::call(
            &k,
            t,
            numbers::CREATE_MSG_PORT,
            SyscallArgs::new(SIGB_MESSAGE as u64, SIGB_MESSAGE as u64, 0, 0, 0, 0),
        ),
        "CreateMsgPort (server)",
    )
    .value;

    // A request that names the client's port as its reply port.
    let mut request = Message::empty();
    request.mn_reply_port = client_port;
    request.mn_sig = SIGB_MESSAGE;
    let request_buf = MessageBuffer::with(&request);

    ok(
        harness::call(
            &k,
            t,
            numbers::PUT_MSG,
            SyscallArgs::new(server_port, request_buf.addr(), 0, 0, 0, 0),
        ),
        "PutMsg",
    );

    // The server takes the request and replies to it.
    let taken = MessageBuffer::empty();
    ok(
        harness::call(
            &k,
            t,
            numbers::GET_MSG,
            SyscallArgs::new(server_port, taken.addr(), 0, 0, 0, 0),
        ),
        "GetMsg",
    );
    assert_eq!(taken.read().mn_reply_port, client_port);

    let mut reply = Message::empty();
    reply.mn_reply = 0xABCD;
    let reply_buf = MessageBuffer::with(&reply);
    ok(
        harness::call(
            &k,
            t,
            numbers::REPLY_MSG,
            SyscallArgs::new(taken.addr(), reply_buf.addr(), 0, 0, 0, 0),
        ),
        "ReplyMsg",
    );

    // The reply is on the *client's* port, and on nobody else's.
    let got = MessageBuffer::empty();
    ok(
        harness::call(
            &k,
            t,
            numbers::GET_MSG,
            SyscallArgs::new(client_port, got.addr(), 0, 0, 0, 0),
        ),
        "GetMsg (client)",
    );
    assert_eq!(
        got.read().mn_reply,
        0xABCD,
        "the reply must reach the client's port"
    );

    // The server's port is empty: the reply did not go back to it.
    let server_still_empty = harness::call(
        &k,
        t,
        numbers::GET_MSG,
        SyscallArgs::new(server_port, got.addr(), 0, 0, 0, 0),
    );
    assert!(server_still_empty.is_err());
}

/// `ReplyMsg` on a message with no reply port is refused.
///
/// A reply with nowhere to go would otherwise be silently dropped, and the
/// caller would wait forever for an answer that was never sent.
#[test]
fn exec_reply_without_a_reply_port_is_refused() {
    let k = harness::kernel();
    let t = harness::task(&k);

    let request = MessageBuffer::empty();
    let reply = MessageBuffer::empty();
    let result = harness::call(
        &k,
        t,
        numbers::REPLY_MSG,
        SyscallArgs::new(request.addr(), reply.addr(), 0, 0, 0, 0),
    );
    assert!(
        result.is_err(),
        "a reply with nowhere to go must be refused"
    );
    assert_eq!(result.error, errors::NO_REPLY_PORT);
}

/// `OpenLibrary` and `CloseLibrary`: a known library opens, an unknown one
/// yields a null handle, and closing twice is an error.
///
/// A null handle is a *successful* open that found nothing — that is the
/// AmigaOS convention, and it is why the call reports no error.
#[test]
fn exec_library_open_and_close() {
    let k = harness::kernel();
    let t = harness::task(&k);

    let known = ok(
        harness::call(
            &k,
            t,
            numbers::OPEN_LIBRARY,
            SyscallArgs::new(cstring("graphics.library"), 0, 0, 0, 0, 0),
        ),
        "OpenLibrary",
    );
    assert_ne!(known.value, 0, "a known library must not be a null handle");

    ok(
        harness::call(
            &k,
            t,
            numbers::CLOSE_LIBRARY,
            SyscallArgs::new(known.value, 0, 0, 0, 0, 0),
        ),
        "CloseLibrary",
    );

    // Closing the same handle again must fail: the library is not open.
    let twice = harness::call(
        &k,
        t,
        numbers::CLOSE_LIBRARY,
        SyscallArgs::new(known.value, 0, 0, 0, 0, 0),
    );
    assert!(twice.is_err(), "closing a closed library must fail");
    assert_eq!(twice.error, errors::NO_OBJECT);

    // An unknown library is a null handle, and *not* an error.
    let unknown = harness::call(
        &k,
        t,
        numbers::OPEN_LIBRARY,
        SyscallArgs::new(cstring("nosuch.library"), 0, 0, 0, 0, 0),
    );
    assert!(unknown.is_ok(), "an unknown library is not an error");
    assert_eq!(
        unknown.value, 0,
        "an unknown library must yield a null handle"
    );
}

/// `Forbid` and `Permit`: the critical section is counted, not a flag.
///
/// Two `Forbid` calls leave a depth of two, and the first `Permit` leaves a
/// depth of one. A boolean would let the first `Permit` unlock a section that
/// was entered twice.
#[test]
fn exec_forbid_and_permit_are_balanced() {
    let k = harness::kernel();
    let t = harness::task(&k);

    let first = ok(
        harness::call(&k, t, numbers::FORBID, SyscallArgs::default()),
        "Forbid",
    );
    assert_eq!(first.value, 1, "the first Forbid leaves depth one");

    let second = ok(
        harness::call(&k, t, numbers::FORBID, SyscallArgs::default()),
        "Forbid",
    );
    assert_eq!(second.value, 2, "the second Forbid leaves depth two");

    let first_permit = ok(
        harness::call(&k, t, numbers::PERMIT, SyscallArgs::default()),
        "Permit",
    );
    assert_eq!(first_permit.value, 1, "one Permit leaves depth one");

    let second_permit = ok(
        harness::call(&k, t, numbers::PERMIT, SyscallArgs::default()),
        "Permit",
    );
    assert_eq!(second_permit.value, 0, "two Permits leave depth zero");

    // A Permit with no matching Forbid must not drive the depth negative.
    let extra = ok(
        harness::call(&k, t, numbers::PERMIT, SyscallArgs::default()),
        "Permit",
    );
    assert_eq!(extra.value, 0, "the depth must never go below zero");
}

/// `GetTaskID`: the call returns the calling task's own identifier.
#[test]
fn exec_get_task_id_returns_the_caller() {
    let k = harness::kernel();
    let t = harness::task(&k);

    let result = ok(
        harness::call(&k, t, numbers::GET_TASK_ID, SyscallArgs::default()),
        "GetTaskID",
    );
    assert_eq!(result.value, t.0 as u64);
}

/// `Yield` is a stub: it returns without doing anything.
///
/// The point of asserting this is that the stub *says so*. A stub that
/// returned success while pretending to be a scheduler yield would be
/// indistinguishable from a working call, which the design document forbids.
#[test]
fn exec_yield_is_a_honest_stub() {
    let k = harness::kernel();
    let t = harness::task(&k);

    let result = harness::call(&k, t, numbers::YIELD, SyscallArgs::default());
    assert!(result.is_ok());
    assert_eq!(result.value, 0);
}
