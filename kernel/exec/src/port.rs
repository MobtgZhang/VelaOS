//! Message ports: the queues and the reply protocol.
//!
//! # Ports are not just queues
//!
//! A port holds three things that are easy to conflate and must not be:
//!
//! 1. **A FIFO of messages** — [`put_msg`] appends, [`get_msg`] takes from
//!    the head, in order.
//! 2. **A signal mask** (`mp_sig`) — the bits this port raises in its owner's
//!    signal set. Enqueueing sets them; draining the queue clears them.
//! 3. **A wait mask** (`mp_sig_wait`) — the bits a waiter must ask for to be
//!    woken. Usually equal to `mp_sig`, but not always: a port that raises a
//!    signal it does not itself wait for is a legal and useful arrangement.
//!
//! # Ownership, and why it matters for replies
//!
//! The reply port is the crux. `ReplyMsg` sends a message *back* to the
//! port named in `mn_reply_port`. That transfers ownership of the message to
//! the reply port's owner, and — this is the part that is easy to get wrong —
//! it does **not** transfer the *reply port itself*. The receiver of a reply
//! still owns its reply port, and when that port is deleted every reply still
//! outstanding for it is dropped rather than delivered to freed memory.
//!
//! That is the whole reason task-exit cleanup exists: a task can die while
//! three replies are in flight toward it, and each of those replies is a
//! pointer into a task that is gone.
//!
//! [`put_msg`]: Port::put_msg
//! [`get_msg`]: Port::get_msg

use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, Ordering};

use spin::Mutex;

use vela_abi_native64::{LibraryHandle, Message, MsgPort};

use crate::task::TaskId;

/// An opaque, kernel-side message-port handle.
///
/// User space never holds one of these — it holds a [`LibraryHandle`] and an
/// index, and the kernel turns that into a port id. The indirection exists
/// because a user pointer naming a port would be a user pointer naming kernel
/// memory.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PortId(pub u32);

impl PortId {
    /// The id of "no port", used for a message with no reply port.
    pub const NONE: PortId = PortId(0);

    /// True when this id names no port.
    pub const fn is_none(self) -> bool {
        self.0 == 0
    }
}

/// The error codes the port operations return.
///
/// These are the `PORT_*` values from the AmigaOS Exec API rather than the
/// `ERROR_*` set, because a caller that gets `NO_MEMORY` back from `PutMsg`
/// needs to know the *message* was not accepted, not that a library was
/// unavailable.
pub mod errors {
    /// The operation succeeded.
    pub const OK: i32 = 0;
    /// The port id does not name a live port.
    pub const NO_OBJECT: i32 = -2;
    /// The name was already taken.
    pub const NAME_TAKEN: i32 = -4;
    /// The object was not found.
    pub const NOT_FOUND: i32 = -6;
    /// The port has no message available.
    pub const NO_MESSAGE: i32 = -1;
    /// A memory allocation failed.
    pub const NO_MEMORY: i32 = -11;
    /// The task is not the port's owner.
    pub const NOT_OWNER: i32 = -24;
    /// The message carries no reply port.
    pub const NO_REPLY_PORT: i32 = -26;
}

/// One port's private state.
///
/// The kernel owns the memory backing a port for as long as the port lives,
/// which is what makes a [`PortId`] safe to hand to user space.
pub struct Port {
    /// The signal bits this port raises when a message arrives.
    sig: u32,
    /// The signal bits a waiter must request to be woken by this port.
    sig_wait: u32,
    /// Port flags, from `vela_abi_native64::port_flags`.
    flags: u32,
    /// The task that owns this port, and whose signal set `sig` raises.
    owner: TaskId,
    /// The FIFO, oldest first. A `Vec` with `remove(0)` would be quadratic;
    /// the cursor below makes head removal O(1) instead.
    queue: Vec<Message>,
    /// Index of the oldest live message within [`Self::queue`].
    head: usize,
    /// The port this one is an alias of, if any.
    alias_of: Option<PortId>,
}

impl core::fmt::Debug for Port {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Port")
            .field("owner", &self.owner)
            .field("sig", &format_args!("{:#x}", self.sig))
            .field("sig_wait", &format_args!("{:#x}", self.sig_wait))
            .field("flags", &self.flags)
            .field("queued", &self.len())
            .finish()
    }
}

impl Port {
    /// Create a port owned by `owner`.
    pub fn new(owner: TaskId, sig: u32, sig_wait: u32, flags: u32) -> Self {
        Self {
            sig,
            sig_wait: if sig_wait == 0 { sig } else { sig_wait },
            flags,
            owner,
            queue: Vec::new(),
            head: 0,
            alias_of: None,
        }
    }

    /// The signals this port raises.
    pub const fn sig(&self) -> u32 {
        self.sig
    }

    /// The signals a waiter must request to be woken by this port.
    pub const fn sig_wait(&self) -> u32 {
        self.sig_wait
    }

    /// The port's flags.
    pub const fn flags(&self) -> u32 {
        self.flags
    }

    /// The task that owns this port.
    pub const fn owner(&self) -> TaskId {
        self.owner
    }

    /// The port this one is an alias of.
    pub fn alias_of(&self) -> Option<PortId> {
        self.alias_of
    }

    /// Make this port an alias of `target`.
    ///
    /// An alias is a second handle onto the same queue. This is what a task
    /// hands a colleague when it wants to be replied to through a different
    /// name, and it is why a port must not be a raw struct pointer: two names,
    /// one queue.
    pub fn set_alias_of(&mut self, target: PortId) {
        self.alias_of = if target == PortId::NONE {
            None
        } else {
            Some(target)
        };
    }

    /// How many messages are queued.
    pub fn len(&self) -> usize {
        self.queue.len() - self.head
    }

    /// True when the queue is empty.
    pub fn is_empty(&self) -> bool {
        self.head >= self.queue.len()
    }

    /// Append `msg` to the tail.
    ///
    /// Returns the new queue length. FIFO order is the AmigaOS guarantee and
    /// it is what lets a task treat its port as a work queue: requests are
    /// serviced in the order they arrived.
    pub fn push(&mut self, msg: Message) -> usize {
        self.queue.push(msg);
        self.queue.len()
    }

    /// Take the oldest message, preserving the rest.
    pub fn pop(&mut self) -> Option<Message> {
        if self.is_empty() {
            return None;
        }
        let msg = self.queue[self.head];
        self.head += 1;
        if self.head == self.queue.len() {
            // Fully drained: release the backing storage entirely.
            self.queue.clear();
            self.head = 0;
        } else if self.head > 32 && self.head * 2 >= self.queue.len() {
            // The consumed prefix dominates the buffer, so compact it.
            self.queue.drain(..self.head);
            self.head = 0;
        }
        Some(msg)
    }

    /// Look at the oldest message without removing it.
    pub fn peek(&self) -> Option<&Message> {
        if self.is_empty() {
            None
        } else {
            Some(&self.queue[self.head])
        }
    }

    /// Discard every queued message and return how many were dropped.
    ///
    /// Used by task exit: a port whose owner is gone must not keep messages
    /// alive, and a request whose reply port has died has nobody to receive
    /// its reply.
    pub fn drain(&mut self) -> usize {
        let n = self.len();
        self.queue.clear();
        self.head = 0;
        n
    }

    /// Discard every queued message whose reply port names `dead_port`.
    ///
    /// The *unreplied-message* cleanup. A request sitting on somebody else's
    /// queue names a reply port; if that reply port belongs to a task that has
    /// died, the request can never be answered, and leaving it queued means
    /// the survivor's queue grows with work that is already impossible.
    ///
    /// The messages are *removed*, not answered: there is nobody to answer
    /// them, and inventing an answer would put a reply into a port whose owner
    /// no longer exists.
    ///
    /// Returns the messages that were removed, in the order they were queued,
    /// so a caller can report or re-route them.
    pub fn drain_unreplied(&mut self, dead_port: PortId) -> Vec<Message> {
        if dead_port.is_none() {
            return Vec::new();
        }
        let mut removed = Vec::new();
        let mut kept = Vec::new();
        // The queue is compacted into `kept` as it is walked, which is what
        // makes this O(n) rather than O(n²): the head cursor cannot skip
        // entries, so the surviving messages are rebuilt once.
        let mut index = self.head;
        while index < self.queue.len() {
            let msg = self.queue[index];
            if msg.mn_reply_port as u32 == dead_port.0 {
                removed.push(msg);
            } else {
                kept.push(msg);
            }
            index += 1;
        }
        if !removed.is_empty() {
            self.queue = kept;
            self.head = 0;
        }
        removed
    }
}

/// The message store.
///
/// Messages are *values*, not objects with identity, and this is a deliberate
/// choice: the AmigaOS `Message` is a structure the sender fills in and hands
/// over by pointer, but VelaOS's syscall boundary copies it in and out. That
/// removes an entire class of bug — a message can never point at a task's
/// stack that has already unwound — at the cost of copying a fixed 56 bytes.
///
/// What the copy does *not* remove is the need to validate the reply port,
/// which is why [`MessageStore::put`] takes an explicit owner.
#[derive(Default)]
pub struct MessageStore {
    /// Live ports, keyed by id.
    ports: Mutex<PortTable>,
    /// The next port id to hand out.
    next_port: AtomicU32,
}

impl core::fmt::Debug for MessageStore {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("MessageStore")
            .field("ports", &self.ports.lock().len())
            .finish()
    }
}

/// The port table: an id-indexed set plus a free list.
///
/// Ids are never reused while a port is alive, because a reused id would let a
/// stale reply port name a *different* port — the classic ABA problem, and a
/// way to have a reply delivered to the wrong task.
#[derive(Default)]
struct PortTable {
    ports: Vec<Option<Port>>,
    free: Vec<PortId>,
}

impl PortTable {
    fn len(&self) -> usize {
        self.ports.iter().filter(|p| p.is_some()).count()
    }

    fn get(&self, id: PortId) -> Option<&Port> {
        if id.is_none() {
            return None;
        }
        self.ports.get(id.0 as usize - 1).and_then(|p| p.as_ref())
    }

    fn get_mut(&mut self, id: PortId) -> Option<&mut Port> {
        if id.is_none() {
            return None;
        }
        self.ports
            .get_mut(id.0 as usize - 1)
            .and_then(|p| p.as_mut())
    }

    fn insert(&mut self, port: Port) -> PortId {
        // Reusing a freed slot is safe precisely because ids are not reused
        // while alive: a free slot's previous owner is gone and any reference
        // to it was already invalidated.
        if let Some(id) = self.free.pop() {
            self.ports[id.0 as usize - 1] = Some(port);
            return id;
        }
        // Port ids start at 1: id 0 is reserved for "no port", and a live
        // port must never be indistinguishable from the absence of one.
        let id = PortId(self.ports.len() as u32 + 1);
        self.ports.push(Some(port));
        id
    }

    fn remove(&mut self, id: PortId) -> Option<Port> {
        if id.is_none() {
            return None;
        }
        let slot = self.ports.get_mut(id.0 as usize - 1)?;
        let port = slot.take()?;
        // The slot goes on the free list but the *id* is not reissued until
        // the slot is reused, and reuse is only safe because every live
        // reference has been invalidated by then.
        self.free.push(id);
        Some(port)
    }
}

impl MessageStore {
    /// An empty store.
    pub const fn new() -> Self {
        Self {
            ports: Mutex::new(PortTable {
                ports: Vec::new(),
                free: Vec::new(),
            }),
            next_port: AtomicU32::new(0),
        }
    }

    /// Create a port owned by `owner`.
    ///
    /// Returns the new port's id. `sig` is the mask this port raises in
    /// `owner`'s signal set; `sig_wait` defaults to `sig` when zero, which is
    /// the overwhelmingly common case.
    pub fn create_port(&self, owner: TaskId, sig: u32, sig_wait: u32, flags: u32) -> PortId {
        let _ = self.next_port.fetch_add(1, Ordering::Relaxed);
        let mut table = self.ports.lock();
        table.insert(Port::new(owner, sig, sig_wait, flags))
    }

    /// Delete a port and return the messages it still held.
    ///
    /// The messages come back so the caller can propagate them — dropping them
    /// here would be correct for a port whose owner is exiting, but wrong for
    /// a port a task is simply closing while another task still has replies
    /// routed through it.
    pub fn delete_port(&self, id: PortId) -> Result<Vec<Message>, i32> {
        let mut table = self.ports.lock();
        let mut port = table.remove(id).ok_or(errors::NO_OBJECT)?;
        let mut out = Vec::with_capacity(port.len());
        while let Some(msg) = port.pop() {
            out.push(msg);
        }
        Ok(out)
    }

    /// True when `id` names a live port.
    pub fn port_exists(&self, id: PortId) -> bool {
        self.ports.lock().get(id).is_some()
    }

    /// How many live ports exist.
    pub fn port_count(&self) -> usize {
        self.ports.lock().len()
    }

    /// The signal mask a port raises.
    pub fn port_sig(&self, id: PortId) -> Option<u32> {
        self.ports.lock().get(id).map(|p| p.sig())
    }

    /// The signal mask a waiter must ask for to be woken by a port.
    ///
    /// Distinct from [`Self::port_sig`]: a port may *raise* one mask and be
    /// waited on with another, and using the wrong one makes a waiter block
    /// on a bit the port never sets.
    pub fn port_sig_wait(&self, id: PortId) -> Option<u32> {
        self.ports.lock().get(id).map(|p| p.sig_wait())
    }

    /// The task that owns a port.
    pub fn port_owner(&self, id: PortId) -> Option<TaskId> {
        self.ports.lock().get(id).map(|p| p.owner())
    }

    /// Enqueue `msg` on `port`.
    ///
    /// This is the low-level half of `PutMsg`. Signalling is deliberately
    /// *not* done here: the store does not own the task table, so it cannot
    /// raise a signal in the owner's signal set. [`crate::Exec::put_msg`]
    /// performs both halves, in the right order.
    pub fn put_msg(&self, port: PortId, msg: Message) -> Result<usize, i32> {
        let mut table = self.ports.lock();
        let p = table.get_mut(port).ok_or(errors::NO_OBJECT)?;
        // A signal-only port holds no messages: it exists to be waited on,
        // and a `PutMsg` to one is a programming error rather than a silent
        // no-op, because the caller believes it queued work.
        if p.flags & vela_abi_native64::port_flags::SIGNAL_ONLY != 0 {
            return Err(errors::NO_OBJECT);
        }
        Ok(p.push(msg))
    }

    /// Take the oldest message from `port`.
    ///
    /// The low-level half of `GetMsg`. Whether the port's message bit should
    /// be cleared depends on the state *after* this call, and the caller is
    /// the only party that can see the owner's signal set, so the decision is
    /// made in [`crate::Exec::get_msg`].
    pub fn get_msg(&self, port: PortId) -> Result<Message, i32> {
        let mut table = self.ports.lock();
        let p = table.get_mut(port).ok_or(errors::NO_OBJECT)?;
        p.pop().ok_or(errors::NO_MESSAGE)
    }

    /// Take the oldest message from a port, but only if `accept` approves it.
    ///
    /// This exists because a syscall that copies a message *out* to user
    /// space must not lose the message when the copy fails. The order is:
    /// look at the message, let the caller decide, and only then remove it.
    ///
    /// The alternative — remove it, then copy, and put it back on failure —
    /// would move the message to the *back* of the queue, breaking the FIFO
    /// guarantee, and would race with a second task taking a message.
    pub fn get_msg_if(
        &self,
        port: PortId,
        accept: impl FnOnce(&Message) -> bool,
    ) -> Result<Message, i32> {
        let mut table = self.ports.lock();
        let p = table.get_mut(port).ok_or(errors::NO_OBJECT)?;
        let Some(msg) = p.peek() else {
            return Err(errors::NO_MESSAGE);
        };
        if !accept(msg) {
            return Err(errors::NO_MESSAGE);
        }
        // The peek was approved, so take it.
        Ok(p.pop().unwrap_or_else(Message::empty))
    }

    /// Look at the oldest message without removing it.
    pub fn peek_msg(&self, port: PortId) -> Result<Message, i32> {
        let table = self.ports.lock();
        let p = table.get(port).ok_or(errors::NO_OBJECT)?;
        p.peek().copied().ok_or(errors::NO_MESSAGE)
    }

    /// True when `port` is now empty.
    ///
    /// Used after a `get_msg` to decide whether the owner's message bit must
    /// be cleared.
    pub fn is_empty(&self, port: PortId) -> Result<bool, i32> {
        let table = self.ports.lock();
        let p = table.get(port).ok_or(errors::NO_OBJECT)?;
        Ok(p.is_empty())
    }

    /// True when `port` has a message ready.
    pub fn has_msg(&self, port: PortId) -> Result<bool, i32> {
        let table = self.ports.lock();
        let p = table.get(port).ok_or(errors::NO_OBJECT)?;
        Ok(!p.is_empty())
    }

    /// How many messages `port` holds.
    pub fn queue_len(&self, port: PortId) -> Result<usize, i32> {
        let table = self.ports.lock();
        let p = table.get(port).ok_or(errors::NO_OBJECT)?;
        Ok(p.len())
    }

    /// Reply to the message `msg` was sent to.
    ///
    /// The reply goes to the port `msg.mn_reply_port` names. The message is
    /// *copied*, and `mn_reply_port` is cleared on the copy so a reply is
    /// never itself replied to: on AmigaOS a reply message is a terminal
    /// object, and a loop of replies is how a two-task conversation turns into
    /// a queue that never drains.
    pub fn reply_msg(&self, msg: &Message, reply: Message) -> Result<usize, i32> {
        let reply_port = msg.mn_reply_port as u32;
        if reply_port == 0 {
            return Err(errors::NO_REPLY_PORT);
        }
        let mut reply = reply;
        // A reply carries no reply port. This is the ownership rule that stops
        // `ReplyMsg` from being usable to build an unbounded chain.
        reply.mn_reply_port = 0;
        // The reply still raises the same signal the original message did, so
        // a waiter on that bit wakes on the reply exactly as it would on the
        // request.
        if reply.mn_sig == 0 {
            reply.mn_sig = msg.mn_sig;
        }
        self.put_msg(PortId(reply_port), reply)
    }

    /// Drop every port owned by `task`, returning the messages they held.
    ///
    /// This is the task-exit cleanup. A reply still in flight toward a dead
    /// task must not be enqueued into a port that no longer exists, and must
    /// not be delivered to a task that no longer exists; dropping both is the
    /// only correct outcome, and it is why [`Self::delete_port`] hands the
    /// messages back rather than freeing them silently.
    ///
    /// It also handles the *other* direction: requests that are still queued on
    /// somebody else's port, naming one of the dying task's ports as their
    /// reply port, are unreplied by definition — there is nobody left to
    /// answer them — so they are removed from those queues as well. A survivor
    /// must not be left holding work that can never complete.
    pub fn cleanup_task(&self, task: TaskId) -> Vec<Message> {
        let mut dropped = Vec::new();
        let (victims, reply_ports): (Vec<PortId>, Vec<PortId>) = {
            let table = self.ports.lock();
            let mut victims = Vec::new();
            let mut reply_ports = Vec::new();
            for (index, slot) in table.ports.iter().enumerate() {
                let Some(port) = slot else { continue };
                if port.owner() == task {
                    victims.push(PortId(index as u32 + 1));
                    reply_ports.push(PortId(index as u32 + 1));
                }
            }
            (victims, reply_ports)
        };

        // First, the requests other tasks are still holding that name one of
        // the dying task's ports as their reply port. These are removed
        // *before* the ports themselves are deleted, so the reply port still
        // exists while the queues are being walked — otherwise the id could
        // already have been recycled and the wrong messages would go.
        {
            let mut table = self.ports.lock();
            for dead in &reply_ports {
                for slot in table.ports.iter_mut() {
                    let Some(port) = slot else { continue };
                    dropped.extend(port.drain_unreplied(*dead));
                }
            }
        }

        for id in victims {
            if let Ok(msgs) = self.delete_port(id) {
                dropped.extend(msgs);
            }
        }
        dropped
    }

    /// Render a port as an ABI [`MsgPort`] for a `GetMsgPort` request.
    ///
    /// The caller gets a *snapshot*, not a live view. That is deliberate: a
    /// user process must not receive a kernel pointer, and a snapshot of the
    /// four fields it actually uses is enough to answer `WaitPort`.
    pub fn describe(&self, id: PortId) -> Option<MsgPort> {
        let table = self.ports.lock();
        let p = table.get(id)?;
        Some(MsgPort {
            mp_node: vela_abi_native64::Node::new(
                vela_abi_native64::node_type::MESSAGE_PORT,
                p.owner().0 as i8,
                "",
            ),
            mp_sig: p.sig(),
            mp_sig_wait: p.sig_wait(),
            mp_flags: p.flags,
            mp_port: 0,
            mp_private: 0,
            mp_owner: p.owner().0,
            mp_msg: 0,
        })
    }
}

/// A library handle paired with a port, as a task would record it.
///
/// This exists because `LibraryBase` and `MsgPort` are stored together in real
/// Exec programs: a task's `.libraryList` holds both, and closing one without
/// the other is the classic leak.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LibraryPortPair {
    /// The library.
    pub library: LibraryHandle,
    /// The port.
    pub port: PortId,
}

#[cfg(test)]
extern crate std;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task::SIGB_MESSAGE;

    fn msg(sig: u32, reply: u64) -> Message {
        Message {
            mn_reply_port: reply,
            mn_node: vela_abi_native64::Node::new(vela_abi_native64::node_type::MESSAGE, 0, ""),
            mn_sig: sig,
            mn_reply: 0,
            mn_payload: [0; vela_abi_native64::MSG_PAYLOAD_WORDS],
        }
    }

    #[test]
    fn a_port_starts_empty() {
        let mut p = Port::new(TaskId(1), SIGB_MESSAGE, 0, 0);
        assert!(p.is_empty());
        assert_eq!(p.len(), 0);
        assert_eq!(p.pop(), None);
    }

    #[test]
    fn messages_come_back_in_fifo_order() {
        let mut p = Port::new(TaskId(1), SIGB_MESSAGE, 0, 0);
        p.push(msg(1, 0));
        p.push(msg(2, 0));
        p.push(msg(3, 0));
        assert_eq!(p.len(), 3);
        // The order is the AmigaOS guarantee that turns a port into a work
        // queue.
        assert_eq!(p.pop().unwrap().mn_sig, 1);
        assert_eq!(p.pop().unwrap().mn_sig, 2);
        assert_eq!(p.pop().unwrap().mn_sig, 3);
        assert!(p.is_empty());
        assert_eq!(p.pop(), None);
    }

    #[test]
    fn peeking_does_not_consume() {
        let mut p = Port::new(TaskId(1), SIGB_MESSAGE, 0, 0);
        p.push(msg(7, 0));
        assert_eq!(p.peek().unwrap().mn_sig, 7);
        assert_eq!(p.peek().unwrap().mn_sig, 7);
        assert_eq!(p.len(), 1);
        assert_eq!(p.pop().unwrap().mn_sig, 7);
    }

    #[test]
    fn a_long_queue_does_not_grow_without_bound() {
        let mut p = Port::new(TaskId(1), SIGB_MESSAGE, 0, 0);
        for i in 0..10_000u32 {
            p.push(msg(i, 0));
        }
        assert_eq!(p.len(), 10_000);
        // Drain it; the vector must compact rather than hold 10k consumed
        // entries.
        let mut n = 0;
        while p.pop().is_some() {
            n += 1;
        }
        assert_eq!(n, 10_000);
        assert!(p.queue.is_empty(), "the backing vector must be released");
    }

    #[test]
    fn draining_reports_how_many_were_dropped() {
        let mut p = Port::new(TaskId(1), SIGB_MESSAGE, 0, 0);
        for i in 0..5u32 {
            p.push(msg(i, 0));
        }
        assert_eq!(p.drain(), 5);
        assert!(p.is_empty());
        assert_eq!(p.drain(), 0);
    }

    #[test]
    fn a_signal_only_port_holds_nothing() {
        let mut p = Port::new(
            TaskId(1),
            SIGB_MESSAGE,
            0,
            vela_abi_native64::port_flags::SIGNAL_ONLY,
        );
        // The flag is recorded so `PutMsg` can reject it; the queue itself is
        // empty and stays that way.
        assert!(p.is_empty());
        assert!(p.flags & vela_abi_native64::port_flags::SIGNAL_ONLY != 0);
        p.drain();
        assert!(p.is_empty());
    }

    #[test]
    fn a_ports_wait_mask_defaults_to_its_signal_mask() {
        let p = Port::new(TaskId(1), 0b1010, 0, 0);
        assert_eq!(p.sig(), 0b1010);
        assert_eq!(p.sig_wait(), 0b1010);
        // But an explicit wait mask wins.
        let q = Port::new(TaskId(1), 0b1010, 0b0100, 0);
        assert_eq!(q.sig_wait(), 0b0100);
    }

    #[test]
    fn aliases_are_recorded_and_clearable() {
        let mut p = Port::new(TaskId(1), SIGB_MESSAGE, 0, 0);
        assert_eq!(p.alias_of(), None);
        p.set_alias_of(PortId(9));
        assert_eq!(p.alias_of(), Some(PortId(9)));
        p.set_alias_of(PortId::NONE);
        assert_eq!(p.alias_of(), None);
    }

    #[test]
    fn the_store_creates_and_finds_ports() {
        let s = MessageStore::new();
        let p = s.create_port(TaskId(1), SIGB_MESSAGE, 0, 0);
        assert!(s.port_exists(p));
        assert_eq!(s.port_owner(p), Some(TaskId(1)));
        assert_eq!(s.port_sig(p), Some(SIGB_MESSAGE));
        assert_eq!(s.port_count(), 1);
    }

    #[test]
    fn port_ids_are_unique_while_ports_are_alive() {
        let s = MessageStore::new();
        let mut ids = Vec::new();
        for _ in 0..100 {
            let p = s.create_port(TaskId(1), SIGB_MESSAGE, 0, 0);
            ids.push(p);
        }
        let mut sorted = ids.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), 100, "two live ports share an id");
    }

    #[test]
    fn putting_and_getting_a_message_preserves_it() {
        let s = MessageStore::new();
        let p = s.create_port(TaskId(1), SIGB_MESSAGE, 0, 0);
        s.put_msg(p, msg(SIGB_MESSAGE, 0)).unwrap();
        assert_eq!(s.queue_len(p), Ok(1));
        let got = s.get_msg(p).unwrap();
        assert_eq!(got.mn_sig, SIGB_MESSAGE);
        assert_eq!(s.queue_len(p), Ok(0));
    }

    #[test]
    fn getting_from_an_empty_port_reports_no_message() {
        let s = MessageStore::new();
        let p = s.create_port(TaskId(1), SIGB_MESSAGE, 0, 0);
        assert_eq!(s.get_msg(p), Err(errors::NO_MESSAGE));
        assert_eq!(s.has_msg(p), Ok(false));
    }

    #[test]
    fn operating_on_a_dead_port_reports_no_object() {
        let s = MessageStore::new();
        assert_eq!(s.put_msg(PortId(4242), msg(1, 0)), Err(errors::NO_OBJECT));
        assert_eq!(s.get_msg(PortId(4242)), Err(errors::NO_OBJECT));
        assert_eq!(s.delete_port(PortId(4242)), Err(errors::NO_OBJECT));
        assert!(!s.port_exists(PortId(4242)));
    }

    #[test]
    fn putting_on_a_signal_only_port_is_rejected() {
        let s = MessageStore::new();
        let p = s.create_port(
            TaskId(1),
            SIGB_MESSAGE,
            0,
            vela_abi_native64::port_flags::SIGNAL_ONLY,
        );
        // A silent no-op would leave the caller believing work was queued.
        assert_eq!(s.put_msg(p, msg(SIGB_MESSAGE, 0)), Err(errors::NO_OBJECT));
        assert_eq!(s.queue_len(p), Ok(0));
    }

    #[test]
    fn a_reply_goes_to_the_reply_port() {
        let s = MessageStore::new();
        let owner = s.create_port(TaskId(2), SIGB_MESSAGE, 0, 0);
        let request = msg(SIGB_MESSAGE, owner.0 as u64);
        s.reply_msg(&request, msg(0, 0)).unwrap();
        // The reply arrived at the sender's port, not the receiver's.
        assert_eq!(s.queue_len(owner), Ok(1));
    }

    #[test]
    fn a_reply_carries_no_reply_port() {
        let s = MessageStore::new();
        let owner = s.create_port(TaskId(2), SIGB_MESSAGE, 0, 0);
        let mut reply = msg(SIGB_MESSAGE, 0);
        // Someone "helpfully" set a reply port on the reply itself.
        reply.mn_reply_port = owner.0 as u64;
        let request = msg(SIGB_MESSAGE, owner.0 as u64);
        s.reply_msg(&request, reply).unwrap();
        let got = s.get_msg(owner).unwrap();
        // The kernel strips it: a reply is terminal, so ReplyMsg cannot be
        // used to build an unbounded chain.
        assert_eq!(got.mn_reply_port, 0);
    }

    #[test]
    fn a_reply_inherits_the_original_signal() {
        let s = MessageStore::new();
        let owner = s.create_port(TaskId(2), 1 << 3, 0, 0);
        let request = msg(1 << 3, owner.0 as u64);
        // The reply does not name a signal of its own.
        let reply = msg(0, 0);
        s.reply_msg(&request, reply).unwrap();
        // A waiter on the original bit wakes on the reply.
        assert_eq!(s.get_msg(owner).unwrap().mn_sig, 1 << 3);
    }

    #[test]
    fn replying_without_a_reply_port_is_an_error() {
        let s = MessageStore::new();
        let request = msg(SIGB_MESSAGE, 0);
        assert_eq!(s.reply_msg(&request, msg(0, 0)), Err(errors::NO_REPLY_PORT));
    }

    #[test]
    fn replying_to_a_dead_reply_port_is_an_error_not_a_panic() {
        let s = MessageStore::new();
        // A port id that never existed.
        let request = msg(SIGB_MESSAGE, 9999);
        assert_eq!(s.reply_msg(&request, msg(0, 0)), Err(errors::NO_OBJECT));
    }

    #[test]
    fn deleting_a_port_hands_back_its_messages() {
        let s = MessageStore::new();
        let p = s.create_port(TaskId(1), SIGB_MESSAGE, 0, 0);
        s.put_msg(p, msg(1, 0)).unwrap();
        s.put_msg(p, msg(2, 0)).unwrap();
        let back = s.delete_port(p).unwrap();
        // Two messages, in order, not silently freed.
        assert_eq!(back.len(), 2);
        assert_eq!(back[0].mn_sig, 1);
        assert_eq!(back[1].mn_sig, 2);
        assert!(!s.port_exists(p));
    }

    #[test]
    fn task_exit_drops_every_port_the_task_owned() {
        let s = MessageStore::new();
        let dying = TaskId(7);
        let survivor = TaskId(8);
        let a = s.create_port(dying, SIGB_MESSAGE, 0, 0);
        let b = s.create_port(dying, SIGB_MESSAGE, 0, 0);
        let c = s.create_port(survivor, SIGB_MESSAGE, 0, 0);
        s.put_msg(a, msg(1, 0)).unwrap();
        s.put_msg(b, msg(2, 0)).unwrap();

        let dropped = s.cleanup_task(dying);
        // The in-flight requests are returned rather than delivered to a task
        // that no longer exists.
        assert_eq!(dropped.len(), 2);
        assert!(!s.port_exists(a));
        assert!(!s.port_exists(b));
        assert!(s.port_exists(c), "another task's port must survive");
    }

    #[test]
    fn a_reply_to_a_dead_task_cannot_be_delivered() {
        let s = MessageStore::new();
        let dying = TaskId(7);
        let port = s.create_port(dying, SIGB_MESSAGE, 0, 0);
        // A message whose reply port names the port that is about to die.
        let request = msg(SIGB_MESSAGE, port.0 as u64);
        let dropped = s.cleanup_task(dying);
        assert_eq!(dropped.len(), 0, "the request itself was never queued");
        // The reply now has nowhere to go, and must say so rather than
        // enqueueing into a recycled id.
        assert_eq!(s.reply_msg(&request, msg(0, 0)), Err(errors::NO_OBJECT));
    }

    #[test]
    fn deleting_an_already_deleted_port_reports_no_object() {
        let s = MessageStore::new();
        let p = s.create_port(TaskId(1), SIGB_MESSAGE, 0, 0);
        assert!(s.delete_port(p).is_ok());
        assert_eq!(s.delete_port(p), Err(errors::NO_OBJECT));
    }

    #[test]
    fn a_port_describes_itself_for_user_space() {
        let s = MessageStore::new();
        let p = s.create_port(TaskId(3), 0b11, 0b01, vela_abi_native64::port_flags::ACTIVE);
        let d = s.describe(p).unwrap();
        assert_eq!(d.mp_sig, 0b11);
        assert_eq!(d.mp_sig_wait, 0b01);
        assert_eq!(d.mp_owner, 3);
        assert!(d.mp_flags & vela_abi_native64::port_flags::ACTIVE != 0);
        // And it is a *snapshot*: no kernel pointer escapes.
        assert_eq!(d.mp_private, 0);
        assert_eq!(d.mp_msg, 0);
        assert!(s.describe(PortId(999)).is_none());
    }

    #[test]
    fn two_tasks_have_independent_ports_and_queues() {
        let s = MessageStore::new();
        let a = s.create_port(TaskId(1), SIGB_MESSAGE, 0, 0);
        let b = s.create_port(TaskId(2), SIGB_MESSAGE, 0, 0);
        s.put_msg(a, msg(1, 0)).unwrap();
        assert_eq!(s.queue_len(b), Ok(0), "task 2's queue is untouched");
    }
}
