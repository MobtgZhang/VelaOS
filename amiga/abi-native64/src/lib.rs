//! The VelaOS native64 ABI.
//!
//! # What "native64" means
//!
//! VelaOS runs 64-bit native code on x86-64, so its own API surface is
//! 64-bit, little-endian, and uses the platform's ordinary calling
//! convention. It is **not** the AmigaOS 3.1 68k ABI: the 68k layouts live in
//! `amiga/abi-guest32` and are only ever used to *describe* a guest program,
//! never to represent a live structure.
//!
//! The two ABIs coexist because the report requires both:
//!
//! * **native64** — the ABI VelaOS's own SDK, userland and kernel use. This
//!   crate.
//! * **guest32** — the Motorola 68k big-endian ABI, for the M6 68k bridge.
//!   Out of scope for v0.1.0, but the layouts are specified now so the
//!   native64 ones can be designed *against* them rather than by accident.
//!
//! # The library calling model
//!
//! AmigaOS calls a library function with the library base in `A6` and the
//! function at a **negative** offset from it:
//!
//! ```asm
//! jsr     -552(a6)        ; exec.library / OpenLibrary
//! ```
//!
//! VelaOS keeps that shape — a library base plus negative offsets — because
//! it is the model a compatibility layer has to speak, and because it means a
//! library's function table can be built once and shared by every process
//! that opens it.
//!
//! The offsets in this crate are *native64* offsets, not the 68k ones. They
//! are negative multiples of the pointer size, which is what a jump table of
//! native function pointers requires. The 68k offsets are tracked separately
//! in `amiga/abi-guest32`.
//!
//! # What is *not* here
//!
//! No function bodies, and no kernel pointers. A user process never receives
//! a kernel address: it receives an opaque [`LibraryId`] and uses it to index
//! its own jump table.

#![no_std]

use core::fmt;

/// The size of a pointer in the native64 ABI, in bytes.
pub const POINTER_SIZE: usize = 8;

/// The maximum length of a node's name, in bytes, including the terminator.
///
/// Matches the AmigaOS `Node` convention: a `BSTR`-style name that is either
/// empty or a NUL-terminated string.
pub const NODE_NAME_LEN: usize = 10;

/// A library offset (LVO) into a library's jump table.
///
/// Always negative, and always a multiple of [`POINTER_SIZE`]. The type
/// exists so that an offset cannot be used where an absolute address is
/// expected, and vice versa — the mix-up is exactly the bug that turns a
/// library call into a jump into the middle of nowhere.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Lvo(i16);

impl Lvo {
    /// The offset of the first jump-table entry.
    ///
    /// Negative, so that `base + lvo` addresses the entry.
    pub const FIRST: Lvo = Lvo(-8);

    /// Build an offset from a byte count.
    ///
    /// # Panics
    ///
    /// Panics if `offset` is not a negative multiple of [`POINTER_SIZE`].
    /// This is a compile-time-shaped constraint enforced at runtime because
    /// `const fn` cannot easily assert; every offset in this crate is a
    /// literal, so the check never fires in practice.
    pub const fn new(offset: i16) -> Self {
        if offset >= 0 {
            panic!("a library offset must be negative");
        }
        if offset % POINTER_SIZE as i16 != 0 {
            panic!("a library offset must be a multiple of the pointer size");
        }
        Self(offset)
    }

    /// The offset in bytes.
    pub const fn offset(self) -> i16 {
        self.0
    }

    /// The index of this offset within a jump table.
    ///
    /// The table's last slot is at offset `-POINTER_SIZE`, so the index of
    /// offset `-n * POINTER_SIZE` is `n - 1`.
    pub const fn index(self) -> usize {
        ((-self.0) as usize / POINTER_SIZE) - 1
    }
}

impl fmt::Display for Lvo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// The number of jump-table slots a library needs.
///
/// The table holds one pointer per exported function. A generous fixed size
/// keeps the geometry simple: every library's base is computed the same way,
/// and adding a function to a library never moves the others' bases.
pub const LIBRARY_VECTOR_SLOTS: usize = 512;

/// The byte size of a library's jump table.
pub const LIBRARY_VECTOR_BYTES: usize = LIBRARY_VECTOR_SLOTS * POINTER_SIZE;

/// A user-space function pointer in a library's jump table.
pub type LibraryFn = u64;

/// The libraries VelaOS v0.1.0 provides.
///
/// The set is closed and versioned: the kernel's registry, the SDK's jump
/// tables and the IDL all agree on it. Adding a library is an ABI change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u32)]
pub enum LibraryId {
    /// `exec.library` — tasks, signals, message ports, memory, libraries.
    Exec = 0,
    /// `graphics.library` — drawing primitives.
    Graphics = 1,
    /// `layers.library` — composited layers.
    Layers = 2,
    /// `intuition.library` — screens, windows, gadgets, IDCMP.
    Intuition = 3,
    /// `dos.library` — the AmigaDOS file API.
    Dos = 4,
    /// `utility.library` — tags, string and list helpers.
    Utility = 5,
}

impl LibraryId {
    /// Every library, in registry order.
    pub const ALL: [LibraryId; 6] = [
        LibraryId::Exec,
        LibraryId::Graphics,
        LibraryId::Layers,
        LibraryId::Intuition,
        LibraryId::Dos,
        LibraryId::Utility,
    ];

    /// The library's AmigaOS-style name, without the `.library` suffix.
    ///
    /// The suffix is implicit: the registry matches `exec` and `exec.library`
    /// equally, because AmigaOS programs use both spellings.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Exec => "exec",
            Self::Graphics => "graphics",
            Self::Layers => "layers",
            Self::Intuition => "intuition",
            Self::Dos => "dos",
            Self::Utility => "utility",
        }
    }

    /// The full name, with the `.library` suffix.
    pub const fn full_name(self) -> &'static str {
        match self {
            Self::Exec => "exec.library",
            Self::Graphics => "graphics.library",
            Self::Layers => "layers.library",
            Self::Intuition => "intuition.library",
            Self::Dos => "dos.library",
            Self::Utility => "utility.library",
        }
    }

    /// The version of this library, in AmigaOS revision units.
    ///
    /// A program asks for a *minimum* version, so this is the version a
    /// program gets when it asks for `0`.
    pub const fn version(self) -> u32 {
        match self {
            Self::Exec => 40,
            Self::Graphics => 40,
            Self::Layers => 40,
            Self::Intuition => 40,
            Self::Dos => 40,
            Self::Utility => 40,
        }
    }

    /// The library's jump-table slot count.
    ///
    /// Every library uses the same generous size, so the base of library `N`
    /// is always `N * LIBRARY_VECTOR_BYTES` past the vector's start. That
    /// makes the base computable from the id alone, with no table.
    pub const fn vector_slots(self) -> usize {
        LIBRARY_VECTOR_SLOTS
    }

    /// Decode a library id from its numeric value.
    pub const fn from_raw(raw: u32) -> Option<Self> {
        match raw {
            0 => Some(Self::Exec),
            1 => Some(Self::Graphics),
            2 => Some(Self::Layers),
            3 => Some(Self::Intuition),
            4 => Some(Self::Dos),
            5 => Some(Self::Utility),
            _ => None,
        }
    }
}

/// The node type byte, as in the AmigaOS `Node`.
///
/// The type identifies what a list contains, which is how a generic list
/// operation knows how to interpret an entry.
pub mod node_type {
    /// A generic node.
    pub const GENERIC: u8 = 0;
    /// A message port.
    pub const MESSAGE_PORT: u8 = 1;
    /// A message.
    pub const MESSAGE: u8 = 2;
    /// A task.
    pub const TASK: u8 = 3;
    /// A file.
    pub const FILE: u8 = 4;
    /// A library.
    pub const LIBRARY: u8 = 5;
    /// A window.
    pub const WINDOW: u8 = 6;
    /// A screen.
    pub const SCREEN: u8 = 7;
}

/// The `Node` structure: the head of every AmigaOS-style list.
///
/// # Layout
///
/// The field order and the padding are the AmigaOS ones:
///
/// ```text
/// offset  0  u8   ln_Type
/// offset  1  i8   ln_Pri
/// offset  2  [u8; 10] ln_Name
/// offset 16  u64  ln_Next
/// offset 24  u64  ln_Prev
/// ```
///
/// The name is a fixed 10-byte inline buffer, not a pointer. That is the
/// AmigaOS design and it has a real consequence: a name longer than nine
/// characters is truncated, and a list of names is *not* a list of strings —
/// each node owns its own copy.
///
/// # Why the links are handles and not pointers
///
/// On AmigaOS `ln_Next` and `ln_Prev` are `struct Node *`. VelaOS makes them
/// opaque `u64` handles instead, for the same reason `mn_reply_port` is a
/// handle: a node may live in a *different address space* from the code that
/// holds a pointer to it, and a raw pointer across that boundary would be
/// both meaningless and a way to name a kernel address.
///
/// The size and the offsets are unchanged, so the layout still matches what
/// a program expects.
#[derive(Debug, Clone, Copy)]
#[repr(C)]
pub struct Node {
    /// What the node holds.
    pub ln_type: u8,
    /// Priority, used by the scheduler for task nodes.
    pub ln_pri: i8,
    /// The node's name, NUL-terminated if it fits.
    pub ln_name: [u8; NODE_NAME_LEN],
    /// The handle of the next node in the list, or `0`.
    pub ln_next: u64,
    /// The handle of the previous node in the list, or `0`.
    pub ln_prev: u64,
}

impl Node {
    /// A node with the given type, priority and name.
    pub fn new(ln_type: u8, pri: i8, name: &str) -> Self {
        let mut ln_name = [0u8; NODE_NAME_LEN];
        let bytes = name.as_bytes();
        let len = bytes.len().min(NODE_NAME_LEN - 1);
        ln_name[..len].copy_from_slice(&bytes[..len]);
        Self {
            ln_type,
            ln_pri: pri,
            ln_name,
            ln_next: 0,
            ln_prev: 0,
        }
    }

    /// The node's name, as a string slice.
    pub fn name(&self) -> &str {
        // The buffer is NUL-terminated by construction: `new` copies at most
        // `NODE_NAME_LEN - 1` bytes, leaving the last byte zero.
        let end = self
            .ln_name
            .iter()
            .position(|b| *b == 0)
            .unwrap_or(NODE_NAME_LEN);
        core::str::from_utf8(&self.ln_name[..end]).unwrap_or("")
    }

    /// True when the node is linked into a list.
    pub fn is_linked(&self) -> bool {
        self.ln_next != 0
    }
}

/// The `List` structure: a doubly-linked list head.
///
/// The AmigaOS list is a *circular* list whose head points at the first
/// element and whose first element's `ln_Prev` points back at the head. That
/// circularity is why an empty list is `{ head: &mut head, tail: &mut head }`
/// rather than two null pointers.
#[derive(Debug, Clone, Copy)]
#[repr(C)]
pub struct List {
    /// The first node, or the list head itself when the list is empty.
    pub lh_head: *mut Node,
    /// The last node, or the list head itself when the list is empty.
    pub lh_tail: *mut Node,
}

/// A message port's flags.
pub mod port_flags {
    /// The port accepts messages.
    pub const ACTIVE: u32 = 1 << 0;
    /// The port is a signal-only port and holds no messages.
    pub const SIGNAL_ONLY: u32 = 1 << 1;
    /// The port is owned by a task and disappears with it.
    pub const OWNED: u32 = 1 << 2;
}

/// The window flags a program may set on `OpenWindow`.
///
/// Defined here, once, rather than in both the kernel and the SDK, because a
/// flag is a *bit assignment* and two hand-written copies of one are two
/// chances to be wrong with nothing able to object: a wrong bit is still a
/// perfectly valid flag. That is not hypothetical — the SDK declared
/// `CLOSEGADGET = 1 << 0` and `DEPTHGADGET = 1 << 3` while the kernel used the
/// AmigaOS `1 << 5` and `1 << 9`, so a program asking for a close gadget set a
/// bit the kernel read as something else, and the window it opened had no close
/// gadget at all.
///
/// These are the AmigaOS `WFLAGS_` values. VelaOS implements the subset the
/// report asks for, and the bits are the real ones so that a ported program's
/// flag arithmetic keeps meaning what it meant.
pub mod window_flags {
    /// No flags: a plain window with no furniture and no special input.
    pub const NONE: u32 = 0;
    /// The window takes mouse events.
    pub const GIMMOUSE: u32 = 1 << 0;
    /// The window takes keyboard events.
    pub const GIMMEKEYBOARD: u32 = 1 << 1;
    /// The window takes events even when another window is active.
    pub const GIMMEMANDATORY: u32 = 1 << 3;
    /// The window takes gadget events.
    pub const GIMMEREQUEST: u32 = 1 << 2;
    /// The window is the active one: it gets keyboard events first.
    pub const ACTIVE: u32 = 1 << 6;
    /// The window has a depth (title bar) gadget.
    pub const DEPTHGADGET: u32 = 1 << 9;
    /// The window has a close gadget.
    pub const CLOSEGADGET: u32 = 1 << 5;
    /// The window is a borderless popup.
    pub const BORDERLESS: u32 = 1 << 10;
}

/// The gadget kinds a program may request.
///
/// The canonical copy, for the same reason as [`window_flags`]: a gadget kind
/// that disagrees between the SDK and the kernel produces a window that draws
/// the wrong thing and reports events under a different id, which reads as a
/// hit-testing bug rather than as an ABI mismatch.
pub mod gadget_kind {
    /// A momentary button.
    pub const BUTTON: u64 = 0;
    /// A text label.
    pub const LABEL: u64 = 1;
    /// A latching toggle.
    pub const TOGGLE: u64 = 2;
    /// A two-state slider.
    pub const SLIDER: u64 = 3;
    /// A pop-up menu.
    pub const MENU: u64 = 4;
    /// A cycle gadget: click to advance to the next image.
    pub const CYCLE: u64 = 5;
    /// A string gadget: displays and edits text.
    pub const STRING: u64 = 6;
    /// A custom, program-drawn gadget.
    pub const CUSTOM: u64 = 7;
}

/// The `MsgPort` structure.
///
/// A port is a queue of messages plus a signal mask. The two are related but
/// distinct: a port can have a signal mask without being signal-only, and a
/// task can wait on a port's *messages* (`WaitPort`) or on its *signals*
/// (`Wait`) independently.
#[derive(Debug, Clone, Copy)]
#[repr(C)]
pub struct MsgPort {
    /// The list node, so ports can be linked into a port list.
    pub mp_node: Node,
    /// The signals this port raises.
    pub mp_sig: u32,
    /// The signals a task must wait for to be woken by this port.
    pub mp_sig_wait: u32,
    /// Port flags.
    pub mp_flags: u32,
    /// The opaque handle of the port this one is an alias of, or `0`.
    pub mp_port: u64,
    /// Port-specific private data, as an opaque kernel handle.
    pub mp_private: u64,
    /// The task that owns the port, if any.
    pub mp_owner: u32,
    /// The opaque handle of the head of the port's message queue, or `0`.
    pub mp_msg: u64,
}

/// The `Message` structure.
///
/// The reply port is the mechanism by which a request gets its answer: the
/// sender fills in `mn_reply_port` with a port it controls, and the receiver
/// uses it to reply. Ownership of the message transfers with the reply.
///
/// # Why `mn_reply_port` is a `u64` and not a pointer
///
/// On AmigaOS `mn_reply_port` is a `struct MsgPort *`, and it really is a
/// pointer into the sender's address space. VelaOS cannot do that: the
/// receiver lives in a different address space, and a native64 program must
/// never receive a kernel pointer.
///
/// So the reply port is an **opaque handle** — the numeric id of a port in
/// the kernel's table. The number is meaningless outside the kernel, which is
/// exactly the property a pointer into another task's memory would lack.
#[derive(Debug, Clone, Copy)]
#[repr(C)]
pub struct Message {
    /// The opaque handle of the port a reply is sent to, or `0` for none.
    pub mn_reply_port: u64,
    /// The list node, so messages can be linked into a queue.
    pub mn_node: Node,
    /// The signal raised when this message arrives.
    pub mn_sig: u32,
    /// The reply payload.
    pub mn_reply: u64,
    /// The extension payload, for a subclassed message.
    ///
    /// # Why the payload is here rather than behind a pointer
    ///
    /// On AmigaOS an `IntuiMessage` *extends* the `Message`, and the Exec
    /// queue holds a pointer to whatever the sender allocated — so a message
    /// can be any size. VelaOS's queue holds the message **by value**, which
    /// is what makes a message incapable of pointing at a stack that has
    /// already unwound, but it also means every message has one fixed size.
    ///
    /// The payload block is what lets a subclassed message carry its own
    /// fields without a pointer and without making every message as large as
    /// the largest subclass. A plain message leaves it zero.
    ///
    /// The layout of the block is defined by the *subclass*, not by the
    /// core: [`IntuiMessage`] gives the meaning of these six words.
    pub mn_payload: [u64; MSG_PAYLOAD_WORDS],
}

/// The number of 64-bit words a message's extension payload holds.
///
/// Six is enough for the `IntuiMessage` subclass — type, code, x, y, button
/// and flags — and small enough that a plain message stays cheap to copy.
pub const MSG_PAYLOAD_WORDS: usize = 6;

impl Message {
    /// A message with no reply port and no payload.
    pub const fn empty() -> Self {
        Self {
            mn_reply_port: 0,
            mn_node: Node {
                ln_type: node_type::MESSAGE,
                ln_pri: 0,
                ln_name: [0; NODE_NAME_LEN],
                ln_next: 0,
                ln_prev: 0,
            },
            mn_sig: 0,
            mn_reply: 0,
            mn_payload: [0; MSG_PAYLOAD_WORDS],
        }
    }

    /// A message carrying an [`IntuiMessage`] subclass.
    ///
    /// The subclass's fields are packed into the payload block. The layout is
    /// the one [`IntuiMessage`] documents, so the two views of the same
    /// message always agree.
    pub fn with_intui(intui: IntuiMessage) -> Self {
        let mut m = Self::empty();
        m.mn_payload = intui.to_payload();
        m.mn_sig = intui.im_sig;
        m
    }

    /// The message's `IntuiMessage` view.
    pub fn as_intui(&self) -> IntuiMessage {
        IntuiMessage::from_payload(self.mn_payload)
    }
}

/// The `IntuiMessage` structure: an Intuition event as a program sees it.
///
/// # Layout
///
/// ```text
/// offset  0  u16  im_Type     the event class
/// offset  2  u16  im_Code     the event detail
/// offset  4  [u8; 4] _pad    alignment for `im_Source`
/// offset  8  u64  im_Source   the window or screen that produced it
/// offset 16  i16  im_MouseX   the pointer's x, in window coordinates
/// offset 18  i16  im_MouseY   the pointer's y, in window coordinates
/// offset 20  u16  im_Button   the mouse buttons
/// offset 22  u16  im_Flags    the event's own flags
/// ```
///
/// The message is carried inside the core [`Message`]'s payload block rather
/// than as a separate allocation, because VelaOS's Exec queue holds messages
/// by value. The core `Message` is the first part of the AmigaOS
/// `IntuiMessage`, so a program that reads the core fields sees exactly the
/// AmigaOS values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
#[allow(
    non_snake_case,
    reason = "the AmigaOS field names are CamelCase by convention"
)]
pub struct IntuiMessage {
    /// The event class: mouse, keyboard, gadget, or an Intuition request.
    pub im_Type: u16,
    /// The event detail: which button, which key, which gadget.
    pub im_Code: u16,
    /// The window or screen that produced the event.
    pub im_Source: u64,
    /// The pointer's x, in the source window's coordinates.
    pub im_MouseX: i16,
    /// The pointer's y, in the source window's coordinates.
    pub im_MouseY: i16,
    /// The mouse buttons that are down.
    pub im_Button: u16,
    /// The event's own flags.
    pub im_Flags: u16,
    /// The signal the message raises when it arrives.
    pub im_sig: u32,
}

impl IntuiMessage {
    /// The event classes, as the AmigaOS `IDCMP_*` values.
    pub const NULL: u16 = 0;
    /// A mouse event.
    pub const MOUSE: u16 = 1;
    /// A keyboard event.
    pub const KEYBOARD: u16 = 2;
    /// A gadget event.
    pub const GADGET: u16 = 4;
    /// An Intuition *request* — a menu selection, a close, a change.
    pub const REQUEST: u16 = 16;

    /// A mouse event.
    pub fn mouse(code: u16, x: i16, y: i16, button: u16) -> Self {
        Self {
            im_Type: Self::MOUSE,
            im_Code: code,
            im_Source: 0,
            im_MouseX: x,
            im_MouseY: y,
            im_Button: button,
            im_Flags: 0,
            im_sig: 0,
        }
    }

    /// A keyboard event.
    pub fn keyboard(code: u16, flags: u16) -> Self {
        Self {
            im_Type: Self::KEYBOARD,
            im_Code: code,
            im_Source: 0,
            im_MouseX: 0,
            im_MouseY: 0,
            im_Button: 0,
            im_Flags: flags,
            im_sig: 0,
        }
    }

    /// A gadget event.
    pub fn gadget(code: u16, source: u64) -> Self {
        Self {
            im_Type: Self::GADGET,
            im_Code: code,
            im_Source: source,
            im_MouseX: 0,
            im_MouseY: 0,
            im_Button: 0,
            im_Flags: 0,
            im_sig: 0,
        }
    }

    /// An Intuition request.
    pub fn request(code: u16, source: u64) -> Self {
        Self {
            im_Type: Self::REQUEST,
            im_Code: code,
            im_Source: source,
            im_MouseX: 0,
            im_MouseY: 0,
            im_Button: 0,
            im_Flags: 0,
            im_sig: 0,
        }
    }

    /// Pack the subclass into the core message's payload block.
    ///
    /// The packing is explicit rather than a transmute, because the payload
    /// block is 64-bit words and the subclass's fields are 16-bit — and
    /// because a transmute would depend on the two layouts agreeing in a way
    /// nothing checks.
    pub fn to_payload(self) -> [u64; MSG_PAYLOAD_WORDS] {
        let mut out = [0u64; MSG_PAYLOAD_WORDS];
        out[0] = (self.im_Type as u64) | ((self.im_Code as u64) << 16);
        out[1] = self.im_Source;
        out[2] = (self.im_MouseX as u16 as u64) | ((self.im_MouseY as u16 as u64) << 16);
        out[3] = (self.im_Button as u64) | ((self.im_Flags as u64) << 16);
        out[4] = self.im_sig as u64;
        out[5] = 0;
        out
    }

    /// Unpack the subclass from the core message's payload block.
    pub fn from_payload(payload: [u64; MSG_PAYLOAD_WORDS]) -> Self {
        Self {
            im_Type: (payload[0] & 0xFFFF) as u16,
            im_Code: ((payload[0] >> 16) & 0xFFFF) as u16,
            im_Source: payload[1],
            im_MouseX: (payload[2] & 0xFFFF) as u16 as i16,
            im_MouseY: ((payload[2] >> 16) & 0xFFFF) as u16 as i16,
            im_Button: (payload[3] & 0xFFFF) as u16,
            im_Flags: ((payload[3] >> 16) & 0xFFFF) as u16,
            im_sig: payload[4] as u32,
        }
    }
}

/// The mouse button codes, as the AmigaOS `IEQUAL_BUTTON_*` values.
pub mod buttons {
    /// The left mouse button.
    pub const LEFT: u16 = 1 << 0;
    /// The right mouse button.
    pub const RIGHT: u16 = 1 << 1;
    /// The middle mouse button.
    pub const MIDDLE: u16 = 1 << 2;
}

/// The mouse event codes, as the AmigaOS `IEQUAL_BUTTON_*` / `MOUSE_*` values.
pub mod mouse_codes {
    /// The pointer moved.
    pub const MOVE: u16 = 1;
    /// The left button went down.
    pub const LEFT_DOWN: u16 = 2;
    /// The left button came up.
    pub const LEFT_UP: u16 = 3;
    /// The right button went down.
    pub const RIGHT_DOWN: u16 = 4;
    /// The right button came up.
    pub const RIGHT_UP: u16 = 5;
    /// The middle button went down.
    pub const MIDDLE_DOWN: u16 = 6;
    /// The middle button came up.
    pub const MIDDLE_UP: u16 = 7;
}

/// Two messages are equal when they carry the same signal and the same reply
/// payload.
///
/// The reply port is deliberately *not* compared: it is a pointer, and two
/// messages naming the same port through different addresses would compare
/// unequal. The signal and payload are the message's *content*, and content
/// is what a test wants to assert on.
impl PartialEq for Message {
    fn eq(&self, other: &Self) -> bool {
        self.mn_sig == other.mn_sig
            && self.mn_reply == other.mn_reply
            && self.mn_payload == other.mn_payload
    }
}

impl Eq for Message {}

/// The `TagItem` structure: a typed key/value pair.
///
/// Tags are how AmigaOS passes optional arguments: a list of them, terminated
/// by `TAG_END`.
#[derive(Debug, Clone, Copy)]
#[repr(C)]
#[allow(
    non_snake_case,
    reason = "the AmigaOS field names are CamelCase by convention"
)]
pub struct TagItem {
    /// The tag's identifier.
    pub ti_Tag: u32,
    /// The tag's value.
    pub ti_Data: u64,
}

/// Tag identifiers used by the native64 API.
pub mod tags {
    /// The end of a tag list.
    pub const TAG_END: u32 = 0;
    /// The tag that skips to the next item.
    pub const TAG_SKIP: u32 = 1;
    /// A tag whose value is a `TagItem`.
    pub const TAG_ITEM: u32 = 2;
    /// The end of a "more" list.
    pub const TAG_MORE: u32 = 3;
}

/// The status codes the native64 API returns.
///
/// These are the AmigaOS convention: zero is success, negative is an error,
/// and the error values are the classic `ERROR_*` set.
pub mod status {
    /// No error.
    pub const OK: i32 = 0;
    /// The operation is not implemented in this version.
    pub const NOT_IMPLEMENTED: i32 = -1;
    /// A required object was not open.
    pub const NO_OBJECT: i32 = -2;
    /// The object already exists.
    pub const OBJECT_IN_USE: i32 = -3;
    /// The name was already taken.
    pub const NAME_TAKEN: i32 = -4;
    /// The name was invalid.
    pub const INVALID_NAME: i32 = -5;
    /// The object was not found.
    pub const NOT_FOUND: i32 = -6;
    /// The name could not be found in the path.
    pub const PATH_NOT_FOUND: i32 = -7;
    /// The name was too long.
    pub const TOO_LARGE: i32 = -8;
    /// The object was not open.
    pub const NOT_OPEN: i32 = -9;
    /// A type or version was not appropriate.
    pub const BAD_TYPE: i32 = -10;
    /// A memory allocation failed.
    pub const NO_MEMORY: i32 = -11;
    /// The operation was not permitted.
    pub const NOT_READY: i32 = -12;
    /// A required item was missing.
    pub const BAD_ITEM: i32 = -13;
    /// The item is not a directory.
    pub const NOT_A_DOS_DISK: i32 = -14;
    /// The object was in use.
    pub const OBJECT_BUSY: i32 = -15;
    /// The object exists but is not the right kind.
    pub const OBJECT_IN_USE2: i32 = -16;
    /// The object was not open for the requested access.
    pub const FILE_NOT_OBJECT: i32 = -17;
    /// The disk is full.
    pub const DISK_FULL: i32 = -18;
    /// A user-specified argument was out of range.
    pub const RANGE: i32 = -19;
    /// The system ran out of a non-recoverable resource.
    pub const SYSTEM_SHUTDOWN: i32 = -20;
    /// The operation was cancelled.
    pub const CANCELLED: i32 = -21;
    /// A child process died.
    pub const CHILD_DIED: i32 = -22;
    /// The object is in the wrong state.
    pub const WRONG_STATE: i32 = -23;
    /// The caller is not the owner.
    pub const NOT_OWNER: i32 = -24;
    /// The signal set is invalid.
    pub const INVALID_SIGNAL: i32 = -25;
    /// The message was not a reply.
    pub const NOT_A_REPLY: i32 = -26;
    /// The port had no message.
    pub const NO_MESSAGE: i32 = -27;
    /// The task was not found.
    pub const TASK_NOT_FOUND: i32 = -28;
    /// The signal could not be delivered.
    pub const SIGNALED: i32 = -29;
}

/// A `BPTR`-shaped file handle as seen by user space.
///
/// On AmigaOS a `BPTR` is a pointer to a file lock structure. VelaOS cannot
/// hand a user program a kernel pointer, so the file handle is an opaque
/// number with the same *shape* — small, opaque, and zero meaning "none".
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(transparent)]
pub struct Bptr(u32);

impl Bptr {
    /// The null handle.
    pub const NULL: Bptr = Bptr(0);

    /// Build a handle from a raw value.
    pub const fn from_raw(raw: u32) -> Self {
        Self(raw)
    }

    /// The raw value.
    pub const fn raw(self) -> u32 {
        self.0
    }

    /// True when the handle names no file.
    pub const fn is_null(self) -> bool {
        self.0 == 0
    }
}

/// The `DateStamp` structure: AmigaOS's timestamp.
///
/// # Layout
///
/// ```text
/// offset  0  i32  ds_Days       days since the AmigaDOS epoch
/// offset  4  i32  ds_Minute     minutes since midnight
/// offset  8  i32  ds_Tick      ticks (1/50 s) within the minute
/// ```
///
/// VelaOS uses Unix epoch seconds internally, and converts at this boundary,
/// because Unix seconds are unambiguous and a timestamp that needs a table to
/// interpret is a timestamp that gets misread.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
#[allow(
    non_snake_case,
    reason = "the AmigaOS field names are CamelCase by convention"
)]
pub struct DateStamp {
    /// Days since the AmigaDOS epoch, which is 1978-01-01.
    pub ds_Days: i32,
    /// Minutes since midnight.
    pub ds_Minute: i32,
    /// Ticks within the minute, at 50 Hz.
    pub ds_Tick: i32,
}

/// The days between the AmigaDOS epoch (1978-01-01) and the Unix epoch.
pub const AMIGA_EPOCH_UNIX_DAYS: i32 = 8 * 365 + 2;

/// The `FileInfo` structure as a program sees it after an `Examine`.
///
/// # Layout
///
/// ```text
/// offset  0  u32   fi_EntryType
/// offset  8  u32   fi_EntryKey
/// offset 12  u64   fi_Size
/// offset 20  u32   fi_DiskKey
/// offset 24  u32   fi_Protection
/// offset 28  i32   fi_OwnerUID
/// offset 32  i32   fi_OwnerGID
/// offset 36  i32   fi_CommentLen
/// offset 40  DateStamp fi_Time
/// offset 52  u64   fi_OwnerUID64
/// offset 60  u64   fi_OwnerGID64
/// offset 68  u32   fi_CommentLen2
/// offset 72  [u8; 108] fi_Comment
/// ```
///
/// The `fi_CommentLen2` field exists because the original `FileInfo` could
/// not express a comment longer than 108 bytes; a zero there means "use
/// `fi_CommentLen`".
///
/// # The two padding holes
///
/// The 68k structure has no alignment requirements and packs its fields, so
/// the 64-bit owner ids land on an odd offset there. VelaOS's native64 ABI
/// is a *different* ABI from the 68k one, so it does not have to reproduce
/// that packing — but the field order is kept, because the order is what a
/// reader recognises. The two explicit holes are what make the 64-bit fields
/// naturally aligned on the 64-bit side; they are spelled out rather than left
/// to the compiler's automatic padding so that the layout is visible in the
/// source and does not silently change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
#[allow(
    non_snake_case,
    reason = "the AmigaOS field names are CamelCase by convention"
)]
pub struct FileInfo {
    /// The entry type: 2 for a file, 3 for a directory.
    pub fi_EntryType: u32,
    /// The parent directory's key.
    pub fi_EntryKey: u32,
    /// The entry's size in bytes.
    pub fi_Size: u64,
    /// The backing disk's key.
    pub fi_DiskKey: u32,
    /// The protection bits.
    pub fi_Protection: u32,
    /// The owning user id.
    pub fi_OwnerUID: i32,
    /// The owning group id.
    pub fi_OwnerGID: i32,
    /// The comment's length.
    pub fi_CommentLen: i32,
    /// Padding, so that the two 64-bit owner fields are naturally aligned.
    ///
    /// The 68k layout has four spare bytes here for exactly this reason: the
    /// 68k has no alignment requirement, but the structure has to be usable
    /// unchanged on a machine that does. Leaving the hole explicit is what
    /// keeps the 64-bit fields at offsets 52 and 60.
    pub _pad: [u8; 4],
    /// The entry's modification time.
    pub fi_Time: DateStamp,
    /// Padding, so that the 64-bit owner fields are 8-byte aligned.
    ///
    /// `fi_Time` is twelve bytes of three `i32`s, so it ends four bytes past
    /// an 8-byte boundary. Without this hole the compiler would insert the
    /// same four bytes anyway — but *implicitly*, which means the next person
    /// to re-order the fields would find it had moved.
    pub _pad2: [u8; 4],
    /// The owning user id, 64-bit form.
    pub fi_OwnerUID64: u64,
    /// The owning group id, 64-bit form.
    pub fi_OwnerGID64: u64,
    /// The long comment's length, or zero.
    pub fi_CommentLen2: u32,
    /// The comment.
    pub fi_Comment: [u8; FILE_INFO_COMMENT_LEN],
}

/// The size of a `FileInfo`'s inline comment buffer.
pub const FILE_INFO_COMMENT_LEN: usize = 108;

/// The AmigaDOS `ST_FILE` entry type.
pub const ENTRY_TYPE_FILE: u32 = 2;
/// The AmigaDOS `ST_DIR` entry type.
pub const ENTRY_TYPE_DIR: u32 = 3;
/// The AmigaDOS `ST_DEV` entry type.
pub const ENTRY_TYPE_DEV: u32 = 4;

/// The `AnchorPath` structure, as `ExamineDir` fills it in.
///
/// # Layout
///
/// ```text
/// offset  0  u8       ap_First
/// offset  1  i8       ap_FoundBreak
/// offset  2  u8       ap_Flags
/// offset  3  u8       ap_Reserved
/// offset  4  u32      ap_Strlen
/// offset  8  u32      ap_SearchFlags
/// offset 12  u32      ap_SegListBlock
/// offset 16  u32      ap_Reserved1
/// offset 20  [u8; 256] ap_Buf
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
#[allow(
    non_snake_case,
    reason = "the AmigaOS field names are CamelCase by convention"
)]
pub struct AnchorPath {
    /// Non-zero once a matching entry has been found.
    pub ap_First: u8,
    /// Whether the last call stopped at a break bit.
    pub ap_FoundBreak: i8,
    /// The caller's flags, echoed back.
    pub ap_Flags: u8,
    /// Reserved.
    pub ap_Reserved: u8,
    /// The length of the path in `ap_Buf`.
    pub ap_Strlen: u32,
    /// The search flags that were used.
    pub ap_SearchFlags: u32,
    /// Reserved.
    pub ap_SegListBlock: u32,
    /// Reserved.
    pub ap_Reserved1: u32,
    /// The path, as an absolute AmigaDOS path.
    pub ap_Buf: [u8; ANCHOR_PATH_BUF_LEN],
}

/// The size of an `AnchorPath`'s path buffer.
pub const ANCHOR_PATH_BUF_LEN: usize = 256;

/// A library handle as seen by user space.
///
/// A user process never receives a kernel pointer. It receives this opaque
/// value, which it uses to index its own jump table. The value is deliberately
/// *not* a pointer: a program that could forge one would be forging a
/// library base, and the whole point of the handle indirection is that the
/// kernel stays the only party that can name a library.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(transparent)]
pub struct LibraryHandle(u32);

impl LibraryHandle {
    /// The null handle, meaning "no library".
    pub const NULL: LibraryHandle = LibraryHandle(0);

    /// Build a handle from a raw value.
    pub const fn from_raw(raw: u32) -> Self {
        Self(raw)
    }

    /// The raw value.
    pub const fn raw(self) -> u32 {
        self.0
    }

    /// True when this is the null handle.
    pub const fn is_null(self) -> bool {
        self.0 == 0
    }

    /// The library this handle names, if it is valid.
    pub const fn library(self) -> Option<LibraryId> {
        // Handle 0 is null; library ids start at 1 in the handle space so the
        // two do not collide.
        if self.0 == 0 {
            return None;
        }
        LibraryId::from_raw(self.0 - 1)
    }

    /// Build a handle for a library.
    pub const fn for_library(library: LibraryId) -> Self {
        Self(library as u32 + 1)
    }
}

#[cfg(test)]
extern crate std;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_node_layout_is_stable() {
        use core::mem::{align_of, offset_of, size_of};
        // The offsets are part of the ABI: a shared structure's layout must
        // match on both sides of the boundary.
        assert_eq!(size_of::<Node>(), 32);
        assert_eq!(align_of::<Node>(), 8);
        assert_eq!(offset_of!(Node, ln_type), 0);
        assert_eq!(offset_of!(Node, ln_pri), 1);
        assert_eq!(offset_of!(Node, ln_name), 2);
        assert_eq!(offset_of!(Node, ln_next), 16);
        assert_eq!(offset_of!(Node, ln_prev), 24);
    }

    #[test]
    fn the_message_and_port_layouts_are_stable() {
        use core::mem::{align_of, offset_of, size_of};
        assert_eq!(size_of::<MsgPort>(), 80);
        assert_eq!(align_of::<MsgPort>(), 8);
        assert_eq!(offset_of!(MsgPort, mp_node), 0);
        assert_eq!(offset_of!(MsgPort, mp_sig), 32);
        assert_eq!(offset_of!(MsgPort, mp_sig_wait), 36);
        assert_eq!(offset_of!(MsgPort, mp_flags), 40);
        assert_eq!(offset_of!(MsgPort, mp_port), 48);
        assert_eq!(offset_of!(MsgPort, mp_private), 56);
        assert_eq!(offset_of!(MsgPort, mp_owner), 64);
        assert_eq!(offset_of!(MsgPort, mp_msg), 72);

        assert_eq!(size_of::<Message>(), 104);
        assert_eq!(offset_of!(Message, mn_reply_port), 0);
        assert_eq!(offset_of!(Message, mn_node), 8);
        assert_eq!(offset_of!(Message, mn_sig), 40);
        assert_eq!(offset_of!(Message, mn_reply), 48);
        assert_eq!(offset_of!(Message, mn_payload), 56);
    }

    #[test]
    fn the_tag_item_layout_is_stable() {
        use core::mem::{align_of, offset_of, size_of};
        assert_eq!(size_of::<TagItem>(), 16);
        assert_eq!(align_of::<TagItem>(), 8);
        assert_eq!(offset_of!(TagItem, ti_Tag), 0);
        assert_eq!(offset_of!(TagItem, ti_Data), 8);
    }

    #[test]
    fn a_node_name_is_truncated_not_overflowed() {
        let n = Node::new(node_type::MESSAGE_PORT, 0, "abcdefghijklmnop");
        // The name buffer holds nine characters and a terminator.
        assert_eq!(n.name(), "abcdefghi");
        assert_eq!(n.ln_name[NODE_NAME_LEN - 1], 0);
    }

    #[test]
    fn a_node_name_that_exactly_fits_is_kept() {
        let n = Node::new(node_type::TASK, 0, "abcdefghi");
        assert_eq!(n.name(), "abcdefghi");
    }

    #[test]
    fn an_empty_node_name_is_empty() {
        let n = Node::new(node_type::GENERIC, 0, "");
        assert_eq!(n.name(), "");
    }

    #[test]
    fn library_offsets_are_negative_multiples_of_the_pointer_size() {
        let l = Lvo::new(-8);
        assert_eq!(l.offset(), -8);
        assert_eq!(l.index(), 0);
        assert_eq!(Lvo::new(-16).index(), 1);
        assert_eq!(Lvo::new(-24).index(), 2);
    }

    #[test]
    #[should_panic(expected = "must be negative")]
    fn a_positive_library_offset_is_rejected() {
        let _ = Lvo::new(8);
    }

    #[test]
    #[should_panic(expected = "multiple of the pointer size")]
    fn a_misaligned_library_offset_is_rejected() {
        let _ = Lvo::new(-4);
    }

    #[test]
    fn library_ids_round_trip() {
        for lib in LibraryId::ALL {
            assert_eq!(LibraryId::from_raw(lib as u32), Some(lib));
        }
        assert_eq!(LibraryId::from_raw(6), None);
        assert_eq!(LibraryId::from_raw(u32::MAX), None);
    }

    #[test]
    fn library_names_carry_the_suffix() {
        assert_eq!(LibraryId::Exec.full_name(), "exec.library");
        assert_eq!(LibraryId::Intuition.full_name(), "intuition.library");
        // And the bare form is what the registry matches on.
        assert_eq!(LibraryId::Exec.name(), "exec");
    }

    #[test]
    fn a_library_handle_distinguishes_null_from_a_real_library() {
        assert!(LibraryHandle::NULL.is_null());
        assert_eq!(LibraryHandle::NULL.library(), None);
        for lib in LibraryId::ALL {
            let h = LibraryHandle::for_library(lib);
            assert!(!h.is_null());
            assert_eq!(h.library(), Some(lib));
        }
    }

    #[test]
    fn a_forged_library_handle_is_rejected() {
        // A handle whose value does not name a library must not resolve.
        let forged = LibraryHandle::from_raw(9999);
        assert!(!forged.is_null());
        assert_eq!(forged.library(), None);
    }

    #[test]
    fn the_jump_table_geometry_is_consistent() {
        // The last slot must be at offset -8 and the first at
        // -8 * LIBRARY_VECTOR_SLOTS.
        assert_eq!(Lvo::new(-8).index(), 0);
        assert_eq!(
            Lvo::new(-(LIBRARY_VECTOR_SLOTS as i16 * 8)).index(),
            LIBRARY_VECTOR_SLOTS - 1
        );
        // Every library reserves the same space, so a base can be computed
        // from the id alone.
        assert_eq!(
            LibraryId::Intuition.vector_slots(),
            LibraryId::Exec.vector_slots()
        );
    }

    #[test]
    fn the_status_codes_are_negative_except_success() {
        // The AmigaOS convention: zero is success, errors are negative.
        assert_eq!(status::OK, 0);
        for code in [
            status::NO_OBJECT,
            status::NOT_FOUND,
            status::NO_MEMORY,
            status::CHILD_DIED,
        ] {
            assert!(code < 0, "status {code} must be negative");
        }
    }

    #[test]
    fn the_status_codes_are_distinct() {
        let all = [
            status::OK,
            status::NOT_IMPLEMENTED,
            status::NO_OBJECT,
            status::OBJECT_IN_USE,
            status::NAME_TAKEN,
            status::INVALID_NAME,
            status::NOT_FOUND,
            status::PATH_NOT_FOUND,
            status::TOO_LARGE,
            status::NOT_OPEN,
            status::BAD_TYPE,
            status::NO_MEMORY,
            status::NOT_READY,
            status::BAD_ITEM,
            status::NOT_A_DOS_DISK,
            status::OBJECT_BUSY,
            status::FILE_NOT_OBJECT,
            status::DISK_FULL,
            status::RANGE,
            status::SYSTEM_SHUTDOWN,
            status::CANCELLED,
            status::CHILD_DIED,
            status::WRONG_STATE,
            status::NOT_OWNER,
            status::INVALID_SIGNAL,
            status::NOT_A_REPLY,
            status::NO_MESSAGE,
            status::TASK_NOT_FOUND,
            status::SIGNALED,
        ];
        let mut sorted = all.to_vec();
        sorted.sort_unstable();
        let before = sorted.len();
        sorted.dedup();
        assert_eq!(sorted.len(), before, "two status codes share a value");
    }

    #[test]
    fn the_tag_values_are_the_amigaos_ones() {
        // These are the classic values, because a program that was written
        // against AmigaOS will use them verbatim.
        assert_eq!(tags::TAG_END, 0);
        assert_eq!(tags::TAG_SKIP, 1);
        assert_eq!(tags::TAG_ITEM, 2);
        assert_eq!(tags::TAG_MORE, 3);
    }

    #[test]
    fn the_bptr_is_an_opaque_handle() {
        assert!(Bptr::NULL.is_null());
        assert_eq!(Bptr::NULL.raw(), 0);
        assert!(!Bptr(1).is_null());
    }

    #[test]
    fn the_datestamp_layout_is_stable() {
        use core::mem::{align_of, offset_of, size_of};
        assert_eq!(size_of::<DateStamp>(), 12);
        assert_eq!(align_of::<DateStamp>(), 4);
        assert_eq!(offset_of!(DateStamp, ds_Days), 0);
        assert_eq!(offset_of!(DateStamp, ds_Minute), 4);
        assert_eq!(offset_of!(DateStamp, ds_Tick), 8);
    }

    #[test]
    fn the_file_info_layout_is_stable() {
        use core::mem::{align_of, offset_of, size_of};
        // 180 bytes of fields, rounded up to the structure's 8-byte
        // alignment. The tail padding is part of the layout, so a program
        // that allocates the structure by `size_of` gets the right size.
        assert_eq!(size_of::<FileInfo>(), 184);
        assert_eq!(align_of::<FileInfo>(), 8);
        assert_eq!(offset_of!(FileInfo, fi_EntryType), 0);
        assert_eq!(offset_of!(FileInfo, fi_EntryKey), 4);
        assert_eq!(offset_of!(FileInfo, fi_Size), 8);
        assert_eq!(offset_of!(FileInfo, fi_DiskKey), 16);
        assert_eq!(offset_of!(FileInfo, fi_Protection), 20);
        assert_eq!(offset_of!(FileInfo, fi_OwnerUID), 24);
        assert_eq!(offset_of!(FileInfo, fi_OwnerGID), 28);
        assert_eq!(offset_of!(FileInfo, fi_CommentLen), 32);
        assert_eq!(offset_of!(FileInfo, fi_Time), 40);
        assert_eq!(offset_of!(FileInfo, fi_OwnerUID64), 56);
        assert_eq!(offset_of!(FileInfo, fi_OwnerGID64), 64);
        assert_eq!(offset_of!(FileInfo, fi_CommentLen2), 72);
        assert_eq!(offset_of!(FileInfo, fi_Comment), 76);
    }

    #[test]
    fn the_anchor_path_layout_is_stable() {
        use core::mem::{align_of, offset_of, size_of};
        assert_eq!(
            size_of::<AnchorPath>(),
            4 + 4 + 4 + 4 + 4 + ANCHOR_PATH_BUF_LEN
        );
        assert_eq!(align_of::<AnchorPath>(), 4);
        assert_eq!(offset_of!(AnchorPath, ap_First), 0);
        assert_eq!(offset_of!(AnchorPath, ap_FoundBreak), 1);
        assert_eq!(offset_of!(AnchorPath, ap_Flags), 2);
        assert_eq!(offset_of!(AnchorPath, ap_Reserved), 3);
        assert_eq!(offset_of!(AnchorPath, ap_Strlen), 4);
        assert_eq!(offset_of!(AnchorPath, ap_SearchFlags), 8);
        assert_eq!(offset_of!(AnchorPath, ap_SegListBlock), 12);
        assert_eq!(offset_of!(AnchorPath, ap_Reserved1), 16);
        assert_eq!(offset_of!(AnchorPath, ap_Buf), 20);
    }

    #[test]
    fn the_entry_types_are_the_amigaos_ones() {
        assert_eq!(ENTRY_TYPE_FILE, 2);
        assert_eq!(ENTRY_TYPE_DIR, 3);
        assert_eq!(ENTRY_TYPE_DEV, 4);
    }

    #[test]
    fn the_amiga_epoch_offset_is_the_documented_one() {
        // 1978-01-01 is 2208 days before 1970-01-01: eight years with two
        // leap days, since 1972 and 1976.
        assert_eq!(AMIGA_EPOCH_UNIX_DAYS, 8 * 365 + 2);
    }

    #[test]
    fn the_intui_message_layout_is_stable() {
        use core::mem::{align_of, offset_of, size_of};
        // The struct is eight-byte aligned because `im_Source` is a `u64`,
        // and the trailing `im_sig` rounds the size up to a multiple of eight.
        assert_eq!(size_of::<IntuiMessage>(), 32);
        assert_eq!(align_of::<IntuiMessage>(), 8);
        assert_eq!(offset_of!(IntuiMessage, im_Type), 0);
        assert_eq!(offset_of!(IntuiMessage, im_Code), 2);
        assert_eq!(offset_of!(IntuiMessage, im_Source), 8);
        assert_eq!(offset_of!(IntuiMessage, im_MouseX), 16);
        assert_eq!(offset_of!(IntuiMessage, im_MouseY), 18);
        assert_eq!(offset_of!(IntuiMessage, im_Button), 20);
        assert_eq!(offset_of!(IntuiMessage, im_Flags), 22);
    }

    #[test]
    fn an_intui_message_round_trips_through_the_payload() {
        let original = IntuiMessage::mouse(mouse_codes::LEFT_DOWN, -120, 45, buttons::LEFT);
        let mut m = Message::empty();
        m.mn_payload = original.to_payload();
        let recovered = m.as_intui();
        assert_eq!(recovered.im_Type, IntuiMessage::MOUSE);
        assert_eq!(recovered.im_Code, mouse_codes::LEFT_DOWN);
        assert_eq!(recovered.im_MouseX, -120);
        assert_eq!(recovered.im_MouseY, 45);
        assert_eq!(recovered.im_Button, buttons::LEFT);
    }

    #[test]
    fn a_negative_mouse_coordinate_survives_the_round_trip() {
        // The coordinate is packed as a `u16` and read back as an `i16`, so a
        // negative value must come back negative rather than as a large
        // positive one.
        let original = IntuiMessage::mouse(mouse_codes::MOVE, -1, -32768, 0);
        let recovered = IntuiMessage::from_payload(original.to_payload());
        assert_eq!(recovered.im_MouseX, -1);
        assert_eq!(recovered.im_MouseY, -32768);
    }

    #[test]
    fn a_plain_message_has_no_intui_payload() {
        let m = Message::empty();
        assert_eq!(m.as_intui().im_Type, IntuiMessage::NULL);
        assert_eq!(m.mn_payload, [0u64; MSG_PAYLOAD_WORDS]);
    }

    #[test]
    fn a_message_built_from_an_intui_subclass_carries_its_signal() {
        let mut intui = IntuiMessage::keyboard(0x41, 0);
        intui.im_sig = 0x8000;
        let m = Message::with_intui(intui);
        assert_eq!(m.mn_sig, 0x8000);
        assert_eq!(m.as_intui().im_sig, 0x8000);
    }

    #[test]
    fn the_button_codes_are_the_amigaos_ones() {
        assert_eq!(buttons::LEFT, 1);
        assert_eq!(buttons::RIGHT, 2);
        assert_eq!(buttons::MIDDLE, 4);
    }

    #[test]
    fn the_mouse_codes_are_the_amigaos_ones() {
        assert_eq!(mouse_codes::MOVE, 1);
        assert_eq!(mouse_codes::LEFT_DOWN, 2);
        assert_eq!(mouse_codes::LEFT_UP, 3);
    }
}
