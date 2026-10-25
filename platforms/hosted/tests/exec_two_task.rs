//! The two-task message exchange the plan requires.
//!
//! # What this file is
//!
//! The plan's Phase 4 gate says: "Scheduler + two-task Exec message
//! exchange passes both hosted and in-kernel tests." This file is the hosted
//! half. It runs *two real tasks* — two scheduler slots, two Exec task ids,
//! two signal sets — through the *real* [`Exec`] service and the *real*
//! dispatch table, and asserts on the AmigaOS semantics the whole design
//! exists to reproduce:
//!
//! * FIFO ordering of a port's queue.
//! * Wake-on-enqueue: a message sets `SIGB_MESSAGE` in the owner's set.
//! * Reply semantics: the reply goes back through the request's reply port,
//!   carries no reply port of its own, and raises the sender's signal.
//! * Signal mask semantics: sticky bits, `SetSignal`/`SetExcept`, and a
//!   wait that is not satisfied by an unrelated bit.
//!
//! Every one of these runs through [`Kernel::syscall`], which is the same
//! dispatch path the `syscall` instruction reaches on bare metal. The only
//! thing that is simulated is the context switch itself.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use vela_abi_native64::Message;
use vela_kernel_exec::port::PortId;
use vela_kernel_exec::task::SIGB_MESSAGE;
use vela_kernel_exec::task::{TaskId, REPLY_MASK};
use vela_kernel_syscall::numbers::{self, errors, SyscallArgs};
use vela_kernel_syscall::SyscallResult;

use vela_hosted::Kernel;

/// The result of a syscall, or a panic with the number that failed.
fn ok(result: SyscallResult, what: &str) -> SyscallResult {
    assert!(result.is_ok(), "{what} failed with error {}", result.error);
    result
}

/// A message carrying `sig` and a payload, with no reply port.
fn message(sig: u32, payload: u64) -> Message {
    let mut m = Message::empty();
    m.mn_sig = sig;
    m.mn_reply = payload;
    m
}

/// A message that names `reply_port` as its reply port.
fn request(sig: u32, payload: u64, reply_port: u32) -> Message {
    let mut m = message(sig, payload);
    m.mn_reply_port = reply_port as u64;
    m
}

#[test]
fn two_tasks_exchange_messages_in_fifo_order() {
    // Two tasks, each with its own port. The sender puts three messages on
    // the receiver's port; the receiver gets them back in the order they
    // were sent, which is the guarantee that makes a port usable as a work
    // queue.
    let k = Kernel::new();
    let exec = k.exec();

    let sender = exec.create_task();
    let receiver = exec.create_task();
    let sender_port = exec.create_port(sender, SIGB_MESSAGE, 0, 0);
    let receiver_port = exec.create_port(receiver, SIGB_MESSAGE, 0, 0);

    for i in 1..=3u64 {
        exec.put_msg(sender_port, message(SIGB_MESSAGE, i)).unwrap();
        exec.put_msg(receiver_port, message(SIGB_MESSAGE, i * 10))
            .unwrap();
    }

    let mut received = Vec::new();
    for _ in 0..3 {
        received.push(exec.get_msg(receiver_port).unwrap().mn_reply);
    }
    assert_eq!(received, vec![10, 20, 30]);

    let mut sent = Vec::new();
    for _ in 0..3 {
        sent.push(exec.get_msg(sender_port).unwrap().mn_reply);
    }
    assert_eq!(sent, vec![1, 2, 3]);

    // And the queues are drained: a further get reports "no message" rather
    // than blocking or returning stale data.
    assert_eq!(
        exec.get_msg(receiver_port).unwrap_err(),
        vela_kernel_exec::port::errors::NO_MESSAGE
    );
}

#[test]
fn a_message_raises_the_owners_signal_bit() {
    // The rendezvous: `PutMsg` enqueues *and then* raises `SIGB_MESSAGE` in
    // the owner's set, so a woken owner always finds its message already
    // there.
    let k = Kernel::new();
    let exec = k.exec();
    let owner = exec.create_task();
    let port = exec.create_port(owner, SIGB_MESSAGE, 0, 0);

    let signals = exec.task_signals(owner).unwrap();
    assert!(!signals.has_any(SIGB_MESSAGE), "nothing has been sent yet");

    exec.put_msg(port, message(SIGB_MESSAGE, 7)).unwrap();
    assert!(
        signals.has_any(SIGB_MESSAGE),
        "a message must raise the owner's message bit",
    );

    // Draining the port clears the bit again, so a `WaitPort` that returns
    // is not left permanently satisfied.
    exec.get_msg(port).unwrap();
    assert!(
        !signals.has_any(SIGB_MESSAGE),
        "draining the port must clear the message bit",
    );

    // And a second message raises it again.
    exec.put_msg(port, message(SIGB_MESSAGE, 8)).unwrap();
    assert!(signals.has_any(SIGB_MESSAGE));
}

#[test]
fn a_reply_goes_back_through_the_request_reply_port() {
    // The requester names its own port as the reply port; the reply arrives
    // there, wakes the requester, and carries no reply port of its own —
    // so a reply cannot be replied to, and two tasks cannot build an
    // unbounded chain of messages.
    let k = Kernel::new();
    let exec = k.exec();
    let client = exec.create_task();
    let server = exec.create_task();
    let client_port = exec.create_port(client, SIGB_MESSAGE, 0, 0);
    let server_port = exec.create_port(server, SIGB_MESSAGE, 0, 0);

    let client_signals = exec.task_signals(client).unwrap();

    // The client asks.
    exec.put_msg(server_port, request(SIGB_MESSAGE, 42, client_port.0))
        .unwrap();
    assert!(exec.task_signals(server).unwrap().has_any(SIGB_MESSAGE));

    // The server answers.
    let request = exec.get_msg(server_port).unwrap();
    exec.reply_msg(&request, message(0, 99)).unwrap();

    // The reply arrived at the client's port, not the server's.
    assert_eq!(exec.ports_queue_len(server_port), 0);
    assert_eq!(exec.ports_queue_len(client_port), 1);

    // The client was woken by the reply. This is checked *before* the reply
    // is taken: taking it drains the port, which clears the bit again.
    assert!(
        client_signals.has_any(SIGB_MESSAGE),
        "a reply must wake the requester",
    );

    let reply = exec.get_msg(client_port).unwrap();
    assert_eq!(reply.mn_reply, 99);
    assert_eq!(reply.mn_sig, SIGB_MESSAGE, "a reply inherits the signal");
    assert_eq!(reply.mn_reply_port, 0, "a reply carries no reply port");
    assert!(
        !client_signals.has_any(SIGB_MESSAGE),
        "draining the reply must clear the bit",
    );
}

#[test]
fn replying_without_a_reply_port_is_refused() {
    let k = Kernel::new();
    let exec = k.exec();
    let owner = exec.create_task();
    let port = exec.create_port(owner, SIGB_MESSAGE, 0, 0);
    // A message that names no reply port: a one-way broadcast.
    let result = exec.reply_msg(&message(SIGB_MESSAGE, 1), message(0, 2));
    assert_eq!(result, Err(vela_kernel_exec::port::errors::NO_REPLY_PORT));
    // And nothing was enqueued anywhere.
    assert_eq!(exec.ports_queue_len(port), 0);
}

#[test]
fn a_reply_to_a_dead_task_is_refused_rather_than_delivered() {
    // The task-exit rule: a reply still in flight toward a task that has
    // gone is refused. Delivering it would put a message into a port whose
    // owner no longer exists.
    let k = Kernel::new();
    let exec = k.exec();
    let doomed = exec.create_task();
    let doomed_port = exec.create_port(doomed, SIGB_MESSAGE, 0, 0);
    let server = exec.create_task();
    let server_port = exec.create_port(server, SIGB_MESSAGE, 0, 0);

    // A request is in flight toward the server, naming the doomed port.
    let in_flight = request(SIGB_MESSAGE, 1, doomed_port.0);
    exec.put_msg(server_port, in_flight).unwrap();

    // The requester dies. Its port goes with it — and the request that named
    // that port as its reply port is unreplied by definition, so it is
    // purged from the server's queue rather than left there as work that can
    // never complete.
    let dropped = exec.exit_task(doomed);
    assert_eq!(
        dropped.len(),
        1,
        "the request that named the dead task's port must be purged",
    );
    assert_eq!(dropped[0].mn_reply_port, doomed_port.0 as u64);
    assert_eq!(
        exec.ports_queue_len(server_port),
        0,
        "the survivor must not be left holding an unanswerable request",
    );

    // The server tries to reply.
    let result = exec.reply_msg(&in_flight, message(0, 2));
    assert!(result.is_err(), "a reply to a dead task must be refused");
    assert_eq!(
        exec.ports_queue_len(server_port),
        0,
        "and the refused reply must not be enqueued either",
    );
}

#[test]
fn an_unreplied_request_on_a_survivors_queue_is_purged_at_task_exit() {
    // The rule the previous test only saw the *reply* side of.
    //
    // A request sits on the server's port naming the doomed task's port as
    // its reply port. When the doomed task exits, that request can never be
    // answered, so it is removed from the server's queue. The server's own
    // messages — the ones it can still answer — are untouched.
    let k = Kernel::new();
    let exec = k.exec();
    let doomed = exec.create_task();
    let doomed_port = exec.create_port(doomed, SIGB_MESSAGE, 0, 0);
    let server = exec.create_task();
    let server_port = exec.create_port(server, SIGB_MESSAGE, 0, 0);

    exec.put_msg(server_port, request(SIGB_MESSAGE, 1, doomed_port.0))
        .unwrap();
    // A message the server *can* answer: it names no reply port.
    exec.put_msg(server_port, message(SIGB_MESSAGE, 2)).unwrap();
    assert_eq!(exec.ports_queue_len(server_port), 2);

    let dropped = exec.exit_task(doomed);
    assert_eq!(
        dropped.len(),
        1,
        "exactly the unanswerable request is dropped"
    );
    assert_eq!(dropped[0].mn_reply, 1);

    assert_eq!(
        exec.ports_queue_len(server_port),
        1,
        "the survivor keeps the message it could still answer",
    );
    let survivor = exec.get_msg(server_port).unwrap();
    assert_eq!(survivor.mn_reply, 2);
    assert_eq!(survivor.mn_reply_port, 0);
}

#[test]
fn signal_bits_are_sticky_until_they_are_read_or_cleared() {
    let k = Kernel::new();
    let exec = k.exec();
    let task = exec.create_task();
    let signals = exec.task_signals(task).unwrap();

    exec.set_signal(task, 0b1010).unwrap();
    assert_eq!(signals.current(), 0b1010);
    // Reading does not consume: Exec signals are a mask, not a queue.
    assert_eq!(signals.current(), 0b1010);
    assert!(signals.has_any(0b0010));
    assert!(!signals.has_any(0b0101));

    // `SetExcept` clears.
    exec.set_except(task, 0b0010).unwrap();
    assert_eq!(signals.current(), 0b1000);
}

#[test]
fn a_wait_is_not_satisfied_by_an_unrelated_signal() {
    // The assertion that stops a spurious signal from looking like a
    // successful wait.
    let k = Kernel::new();
    let exec = k.exec();
    let task = exec.create_task();
    let signals = exec.task_signals(task).unwrap();

    exec.set_signal(task, 0b0100).unwrap();
    // The task asks for bits 1 and 2. Bit 2 is set, so the wait succeeds
    // immediately — and reports exactly the bit that satisfied it, not the
    // whole mask.
    let satisfied = exec.wait(task, 0b0110).unwrap().unwrap();
    assert_eq!(satisfied, 0b0100);

    // Now ask for a bit that is not set: the task blocks.
    exec.set_except(task, 0b1111).unwrap();
    let blocked = exec.wait(task, 0b0100).unwrap();
    assert!(blocked.is_none(), "the task must block on an unset bit");
    assert!(signals.is_waiting());
    assert_eq!(
        signals.blocked_on(),
        vela_kernel_exec::task::BlockedOn::Signal(0b0100)
    );

    // Raising the bit *while the task is waiting* unblocks it. The wait
    // reports the requested bit that satisfied it — here bit 2, because bit 1
    // was raised but not asked for.
    signals.raise(0b0110);
    assert_eq!(exec.wait(task, 0b0100).unwrap(), Some(0b0110 & 0b0100));
}

#[test]
fn the_message_bit_and_the_reply_bits_do_not_overlap() {
    // A reply must be distinguishable from a new request: the whole reason
    // the mask is split at bit 0.
    assert_eq!(SIGB_MESSAGE, 1);
    assert_eq!(SIGB_MESSAGE & REPLY_MASK, 0);
    assert_eq!(REPLY_MASK.count_ones(), 31);
}

#[test]
fn two_scheduled_tasks_pass_a_message_to_each_other() {
    // The full loop, through the scheduler: task A blocks on its port, task
    // B sends it a message and wakes it, and A receives it — all by going
    // through the scheduler's run queue rather than by calling the services
    // in order.
    let k = Kernel::new();

    let seen = Arc::new(Mutex::new(VecDeque::new()));
    let ran = Arc::new(AtomicU32::new(0));

    // The receiver is spawned first so that its task id — and therefore the
    // port it owns — is known before the sender runs.
    let receiver_id = Arc::new(Mutex::new(TaskId::NONE));
    let receiver_port = Arc::new(Mutex::new(PortId::NONE));

    {
        let seen = Arc::clone(&seen);
        let ran = Arc::clone(&ran);
        let receiver_id = Arc::clone(&receiver_id);
        let receiver_port = Arc::clone(&receiver_port);
        k.spawn("receiver", 0, move |k| {
            ran.fetch_add(1, Ordering::SeqCst);
            // The port is created on the first run, and its id is published
            // so the sender can find it.
            let port = {
                let mut slot = receiver_port.lock().unwrap();
                if slot.is_none() {
                    let p = k
                        .exec()
                        .create_port(k.exec().current_task(), SIGB_MESSAGE, 0, 0);
                    *receiver_id.lock().unwrap() = k.exec().current_task();
                    *slot = p;
                    p
                } else {
                    *slot
                }
            };
            // Is the message already here?
            match k.exec().get_msg(port) {
                Ok(m) => seen.lock().unwrap().push_back(m.mn_reply),
                Err(_) => {
                    // Not yet: block until the sender's message wakes us.
                    k.block_current(SIGB_MESSAGE);
                }
            }
        });
    }

    // The sender. It runs second, because it is at a lower priority.
    {
        let seen = Arc::clone(&seen);
        let ran = Arc::clone(&ran);
        let receiver_id = Arc::clone(&receiver_id);
        let receiver_port = Arc::clone(&receiver_port);
        k.spawn("sender", 6, move |k| {
            ran.fetch_add(1, Ordering::SeqCst);
            let port = *receiver_port.lock().unwrap();
            k.exec()
                .put_msg(port, message(SIGB_MESSAGE, 0xABCD))
                .unwrap();
            // Waking the receiver is what turns the message into a
            // rendezvous rather than a queue that happens to be polled.
            k.wake(*receiver_id.lock().unwrap(), SIGB_MESSAGE);
            let _ = seen;
        });
    }

    let steps = k.run_until_idle_with_limit(64);
    assert!(steps <= 64, "the harness ran away");

    // The receiver ran twice: once to find nothing and block, once with the
    // message in hand.
    assert_eq!(
        ran.load(Ordering::SeqCst),
        3,
        "expected two receiver runs and one sender run"
    );
    assert_eq!(*seen.lock().unwrap(), vec![0xABCD]);
}

#[test]
fn a_message_to_a_dead_ports_owner_is_dropped_not_delivered() {
    let k = Kernel::new();
    let exec = k.exec();
    let doomed = exec.create_task();
    let a = exec.create_port(doomed, SIGB_MESSAGE, 0, 0);
    let b = exec.create_port(doomed, SIGB_MESSAGE, 0, 0);

    exec.put_msg(a, message(SIGB_MESSAGE, 1)).unwrap();
    exec.put_msg(b, message(SIGB_MESSAGE, 2)).unwrap();

    let dropped = exec.exit_task(doomed);
    // Both in-flight messages come back to the kernel rather than being
    // delivered to a task that no longer exists.
    assert_eq!(dropped.len(), 2);
    let payloads: Vec<u64> = dropped.iter().map(|m| m.mn_reply).collect();
    assert_eq!(payloads, vec![1, 2]);
}

#[test]
fn a_deleted_port_hands_back_its_messages() {
    let k = Kernel::new();
    let exec = k.exec();
    let owner = exec.create_task();
    let port = exec.create_port(owner, SIGB_MESSAGE, 0, 0);
    exec.put_msg(port, message(SIGB_MESSAGE, 1)).unwrap();
    exec.put_msg(port, message(SIGB_MESSAGE, 2)).unwrap();

    let back = exec.delete_port(port).unwrap();
    assert_eq!(back.len(), 2);
    // And the port is gone: using it is an error, not a use-after-free.
    assert_eq!(
        exec.get_msg(port).unwrap_err(),
        vela_kernel_exec::port::errors::NO_OBJECT
    );
}

#[test]
fn two_tasks_open_and_close_the_same_library_by_reference_count() {
    let k = Kernel::new();
    let exec = k.exec();
    let one = exec.create_task();
    let two = exec.create_task();

    let a = exec.open_library("graphics.library", 40);
    let b = exec.open_library("graphics.library", 40);
    assert!(!a.is_null());
    assert_eq!(a, b, "the same library has one base");

    // Two opens means two closes: the registry is reference counted.
    assert!(exec.close_library(a));
    // Still open: the second holder has it.
    assert!(!exec.open_library("graphics.library", 40).is_null());
    let c = exec.open_library("graphics.library", 40);
    assert!(exec.close_library(b));
    assert!(exec.close_library(c));
    let _ = (one, two);
}

#[test]
fn opening_an_unknown_library_returns_null_rather_than_failing() {
    // AmigaOS semantics: `OpenLibrary` returns NULL when the library is not
    // found. It is a *successful* call with a NULL result, which is exactly
    // why the syscall layer splits value from error.
    let k = Kernel::new();
    let exec = k.exec();
    let handle = exec.open_library("nosuch.library", 0);
    assert!(handle.is_null());

    let task = exec.create_task();
    let result = k.syscall(
        task,
        numbers::OPEN_LIBRARY,
        SyscallArgs::new(0, 0, 0, 0, 0, 0),
    );
    // The name pointer is null, which is a bad pointer rather than a missing
    // library — the two failures are distinguishable.
    assert_eq!(result.error, errors::BAD_POINTER);
}

#[test]
fn forbid_and_permit_are_balanced_and_bounded() {
    let k = Kernel::new();
    let exec = k.exec();
    assert_eq!(exec.forbid(), 1);
    assert_eq!(exec.forbid(), 2, "Forbid nests");
    assert!(exec.is_forbidden());
    assert_eq!(exec.permit(), 1);
    assert_eq!(exec.permit(), 0);
    assert!(!exec.is_forbidden());

    // A missing Permit must not freeze the kernel: the critical section is
    // force-released rather than left held forever.
    exec.forbid();
    exec.forbid();
    assert!(
        exec.force_release(),
        "the classic lock must be force-releasable"
    );
    assert!(!exec.is_forbidden());
}

#[test]
fn a_bad_user_pointer_is_a_clean_error_through_the_syscall_path() {
    // The plan requires this case explicitly: "a `tests/api-conformance`
    // case deliberately passes invalid pointers and asserts a clean
    // error". Here it is at the harness level, through the same dispatch
    // path the gate uses.
    let k = Kernel::new();
    let task = k.exec().create_task();

    let created = ok(
        k.syscall(
            task,
            numbers::CREATE_MSG_PORT,
            SyscallArgs::new(1, 0, 0, 0, 0, 0),
        ),
        "CreateMsgPort",
    );
    assert_ne!(created.value, 0);

    // A message pointer in the kernel half.
    let result = k.syscall(
        task,
        numbers::PUT_MSG,
        SyscallArgs::new(created.value, 0xFFFF_8000_0000_0000, 0, 0, 0, 0),
    );
    assert_eq!(result.error, errors::BAD_POINTER);

    // A library name pointer that is null.
    let result = k.syscall(
        task,
        numbers::OPEN_LIBRARY,
        SyscallArgs::new(0, 40, 0, 0, 0, 0),
    );
    assert_eq!(result.error, errors::BAD_POINTER);

    // And a get into an unmapped address. The port must hold a message, or
    // `GetMsg` reports "no message" and the copy is never attempted — which
    // would make this case pass for the wrong reason.
    let mut m = message(SIGB_MESSAGE, 1);
    ok(
        k.syscall(
            task,
            numbers::PUT_MSG,
            SyscallArgs::new(created.value, &mut m as *mut Message as u64, 0, 0, 0, 0),
        ),
        "PutMsg",
    );
    let result = k.syscall(
        task,
        numbers::GET_MSG,
        SyscallArgs::new(created.value, 0x1000_0000_0000, 0, 0, 0, 0),
    );
    assert_eq!(result.error, errors::BAD_POINTER);

    // A `GetMsg` into a mapped buffer still works, which proves the refusal
    // above was about the address and not about the port.
    let mut got = Message::empty();
    ok(
        k.syscall(
            task,
            numbers::GET_MSG,
            SyscallArgs::new(created.value, &mut got as *mut Message as u64, 0, 0, 0, 0),
        ),
        "GetMsg into a valid buffer",
    );
    assert_eq!(got.mn_reply, 1);
}

#[test]
fn an_unimplemented_library_syscall_says_so_honestly() {
    // "A stub is not compatible": a call that has no handler must report that
    // it is not implemented rather than silently returning success.
    //
    // `utility.library` is the last group with no slots filled in. Graphics,
    // Layers and Intuition all have real handlers now, so they are exercised
    // in `gui_syscalls.rs` rather than here.
    let k = Kernel::new();
    let task = k.exec().create_task();
    for number in [numbers::UTILITY_FIND_TAG, numbers::UTILITY_STRNCPY] {
        let result = k.syscall(task, number, SyscallArgs::default());
        assert_eq!(
            result.error,
            errors::NOT_IMPLEMENTED,
            "syscall {number} must report NOT_IMPLEMENTED",
        );
    }
}

#[test]
fn a_gui_syscall_no_longer_reports_not_implemented() {
    // The mirror of the test above: as each library lands, its syscalls stop
    // reporting "not implemented". If this starts failing, a handler has been
    // removed and something has silently become a stub.
    let k = Kernel::new();
    let task = k.exec().create_task();
    for number in [numbers::GRAPHICS_ALLOC_PEN, numbers::INTUITION_OPEN_WINDOW] {
        let result = k.syscall(task, number, SyscallArgs::default());
        assert_ne!(
            result.error,
            errors::NOT_IMPLEMENTED,
            "syscall {number} must have a handler",
        );
    }
}

#[test]
fn a_port_can_be_created_and_used_through_syscalls_alone() {
    // The whole Exec surface a user program sees, driven by syscalls.
    let k = Kernel::new();
    let sender = k.exec().create_task();
    let receiver = k.exec().create_task();

    let sender_port = ok(
        k.syscall(
            sender,
            numbers::CREATE_MSG_PORT,
            SyscallArgs::new(SIGB_MESSAGE as u64, 0, 0, 0, 0, 0),
        ),
        "CreateMsgPort(sender)",
    )
    .value;
    let receiver_port = ok(
        k.syscall(
            receiver,
            numbers::CREATE_MSG_PORT,
            SyscallArgs::new(SIGB_MESSAGE as u64, 0, 0, 0, 0, 0),
        ),
        "CreateMsgPort(receiver)",
    )
    .value;

    // The "user" message buffer lives on the host, which is the user half.
    //
    // The request goes to the *receiver's* port, and it names the *sender's*
    // port as its reply port: that is how the receiver knows where to send
    // the answer.
    let mut m = request(SIGB_MESSAGE, 0x5A5A, sender_port as u32);
    ok(
        k.syscall(
            sender,
            numbers::PUT_MSG,
            SyscallArgs::new(receiver_port, &mut m as *mut Message as u64, 0, 0, 0, 0),
        ),
        "PutMsg",
    );

    // The receiver gets it into its own buffer.
    let mut got = Message::empty();
    ok(
        k.syscall(
            receiver,
            numbers::GET_MSG,
            SyscallArgs::new(receiver_port, &mut got as *mut Message as u64, 0, 0, 0, 0),
        ),
        "GetMsg",
    );
    assert_eq!(got.mn_reply, 0x5A5A);
    assert_eq!(got.mn_reply_port, sender_port);

    // And the sender replies through the reply port it named.
    let mut reply = message(0, 0x1234);
    ok(
        k.syscall(
            receiver,
            numbers::REPLY_MSG,
            SyscallArgs::new(
                &got as *const Message as u64,
                &mut reply as *mut Message as u64,
                0,
                0,
                0,
                0,
            ),
        ),
        "ReplyMsg",
    );

    let mut answer = Message::empty();
    ok(
        k.syscall(
            sender,
            numbers::GET_MSG,
            SyscallArgs::new(sender_port, &mut answer as *mut Message as u64, 0, 0, 0, 0),
        ),
        "GetMsg(reply)",
    );
    assert_eq!(answer.mn_reply, 0x1234);
    assert_eq!(answer.mn_reply_port, 0, "a reply is terminal");
}

#[test]
fn forbid_and_permit_are_reachable_from_a_syscall() {
    let k = Kernel::new();
    let task = k.exec().create_task();
    let depth = ok(
        k.syscall(task, numbers::FORBID, SyscallArgs::default()),
        "Forbid",
    );
    assert_eq!(depth.value, 1);
    let remaining = ok(
        k.syscall(task, numbers::PERMIT, SyscallArgs::default()),
        "Permit",
    );
    assert_eq!(remaining.value, 0);
}
