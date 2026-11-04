//! The `exec.library` conformance behaviour tests.
//!
//! Exec is the heart of the AmigaOS semantics VelaOS reproduces: message
//! ports, FIFO message queues, reply ownership, signal masks, the classic
//! critical section and the library registry. Every test here drives the
//! **real** Exec service through the **real** syscall dispatch table, using
//! the hosted mock kernel.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use vela_hal_common::{MapError, PAGE_SIZE};
use vela_test_api_conformance::harness;

use vela_abi_native64::Message;
use vela_kernel_exec::task::SIGB_MESSAGE;
use vela_kernel_syscall::numbers::{self, errors, SyscallArgs};
use vela_kernel_syscall::task_space::{self, FrameSource, BREAK_START, IMAGE_BASE};

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

/// `Yield` gives up the rest of the task's time slice.
///
/// The handler only records the request; the scheduler honours it after the
/// task body returns. Two equal-priority tasks therefore interleave as
/// `a, b, a` rather than either running its body twice in a row — and the
/// first `a` does not immediately run again, which is the whole difference
/// between a yield and an ordinary successful call.
#[test]
fn exec_yield_requeues_the_task_after_its_body() {
    let k = harness::kernel();
    let order = Arc::new(Mutex::new(Vec::new()));
    let runs = Arc::new(AtomicUsize::new(0));

    {
        let order = Arc::clone(&order);
        let runs = Arc::clone(&runs);
        k.spawn("yielder", 0, move |k| {
            order.lock().unwrap().push('a');
            // Yield exactly once: the second body run must finish, so the
            // harness terminates rather than requeueing the task forever.
            if runs.fetch_add(1, Ordering::SeqCst) == 0 {
                let id = k.exec().current_task();
                ok(
                    k.syscall(id, numbers::YIELD, SyscallArgs::default()),
                    "Yield",
                );
            }
        });
    }
    {
        let order = Arc::clone(&order);
        k.spawn("other", 0, move |_k| {
            order.lock().unwrap().push('b');
        });
    }

    let steps = k.run_until_idle_with_limit(64);
    assert!(steps <= 64, "the harness ran away");
    assert_eq!(
        *order.lock().unwrap(),
        vec!['a', 'b', 'a'],
        "a yield must put the caller behind its equal-priority peer"
    );
}

/// `AddPort` reports the port's whole signal mask, and refuses a port that
/// is not the caller's.
///
/// The mask is what a caller builds its wait from, so returning one bit
/// would make it sleep through the other bits the port raises. Ownership is
/// checked after existence, so a dead handle reports `NO_OBJECT` and a live
/// handle owned by someone else reports `NOT_OWNER`.
#[test]
fn exec_add_port_reports_the_ports_signal_mask() {
    let k = harness::kernel();
    let t = harness::task(&k);
    let other = harness::task(&k);

    // A port that raises two bits. The whole mask must come back.
    let signal = SIGB_MESSAGE | 0b0100;
    let port = ok(
        harness::call(
            &k,
            t,
            numbers::CREATE_MSG_PORT,
            SyscallArgs::new(signal as u64, 0, 0, 0, 0, 0),
        ),
        "CreateMsgPort",
    )
    .value;

    let mask = ok(
        harness::call(
            &k,
            t,
            numbers::ADD_PORT,
            SyscallArgs::new(port, 0, 0, 0, 0, 0),
        ),
        "AddPort",
    );
    assert_eq!(mask.value, signal as u64);

    // Someone else's port is live but not the caller's.
    let refused = harness::call(
        &k,
        other,
        numbers::ADD_PORT,
        SyscallArgs::new(port, 0, 0, 0, 0, 0),
    );
    assert!(refused.is_err(), "a foreign port must be refused");
    assert_eq!(refused.error, errors::NOT_OWNER);

    // A handle that names no port at all is a different refusal.
    let missing = harness::call(
        &k,
        t,
        numbers::ADD_PORT,
        SyscallArgs::new(999_999, 0, 0, 0, 0, 0),
    );
    assert!(missing.is_err(), "a missing port must be refused");
    assert_eq!(missing.error, errors::NO_OBJECT);
}

/// `MapPage` maps one page for the calling task.
#[test]
fn exec_map_page_maps_a_page_for_the_caller() {
    // The space registry and the installed frame source are process-wide, so
    // the memory tests run one at a time.
    let _guard = memory_serial();
    let k = harness::kernel();
    let t = harness::task(&k);
    task_space::spaces().create(t);
    let frames = install_fake_frames(8);

    let mapped = ok(
        harness::call(
            &k,
            t,
            numbers::MAP_PAGE,
            SyscallArgs::new(IMAGE_BASE, 0, 0, 0, 0, 0),
        ),
        "MapPage",
    );
    assert_eq!(mapped.value, IMAGE_BASE);
    assert_eq!(
        frames.remaining(),
        7,
        "mapping one page must consume exactly one frame"
    );
}

/// `Brk` grows the break and shrinks it back, consuming and returning frames.
#[test]
fn exec_brk_grows_and_shrinks_the_break() {
    let _guard = memory_serial();
    let k = harness::kernel();
    let t = harness::task(&k);
    task_space::spaces().create(t);
    let frames = install_fake_frames(16);

    // `brk(0)` queries without moving the break.
    let query = ok(
        harness::call(&k, t, numbers::BRK, SyscallArgs::new(0, 0, 0, 0, 0, 0)),
        "Brk (query)",
    );
    assert_eq!(query.value, BREAK_START);

    // Grow by two pages.
    let target = BREAK_START + 2 * PAGE_SIZE;
    let grown = ok(
        harness::call(&k, t, numbers::BRK, SyscallArgs::new(target, 0, 0, 0, 0, 0)),
        "Brk (grow)",
    );
    assert_eq!(grown.value, target);
    assert_eq!(frames.remaining(), 14);

    // Shrink back to the base: the two pages are returned.
    let shrunk = ok(
        harness::call(
            &k,
            t,
            numbers::BRK,
            SyscallArgs::new(BREAK_START, 0, 0, 0, 0, 0),
        ),
        "Brk (shrink)",
    );
    assert_eq!(shrunk.value, BREAK_START);
    assert_eq!(frames.remaining(), 16);
}

/// A frame source that hands out page-aligned, zeroed host allocations.
///
/// The mapped page is zeroed through its *physical* address, so a fake frame
/// must be a real, writable, page-aligned allocation. A budget bounds how
/// many frames it can hand out, and `free_frame` restores one.
struct FakeFrames {
    budget: Mutex<u64>,
    pages: Mutex<Vec<u64>>,
}

impl FakeFrames {
    fn new(budget: u64) -> Self {
        Self {
            budget: Mutex::new(budget),
            pages: Mutex::new(Vec::new()),
        }
    }

    fn remaining(&self) -> u64 {
        *self.budget.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl FrameSource for FakeFrames {
    fn alloc_frame(&self) -> Result<u64, MapError> {
        let mut budget = self.budget.lock().unwrap_or_else(|e| e.into_inner());
        if *budget == 0 {
            return Err(MapError::OutOfMemory);
        }
        *budget -= 1;

        let layout = std::alloc::Layout::from_size_align(PAGE_SIZE as usize, PAGE_SIZE as usize)
            .map_err(|_| MapError::OutOfMemory)?;
        let raw = unsafe { std::alloc::alloc_zeroed(layout) };
        if raw.is_null() {
            return Err(MapError::OutOfMemory);
        }
        let addr = raw as u64;
        self.pages
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(addr);
        Ok(addr)
    }

    unsafe fn free_frame(&self, _pa: u64) {
        *self.budget.lock().unwrap_or_else(|e| e.into_inner()) += 1;
    }
}

/// Install a fake frame source for one memory test.
fn install_fake_frames(budget: u64) -> &'static FakeFrames {
    let frames: &'static FakeFrames = Box::leak(Box::new(FakeFrames::new(budget)));
    // SAFETY: the source is leaked, so it outlives every user space the test
    // creates; the memory tests are serial, so no other test installs one.
    unsafe { task_space::install_frames(frames) };
    frames
}

/// Serialise the tests that share the process-wide space registry and frames.
fn memory_serial() -> MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}
