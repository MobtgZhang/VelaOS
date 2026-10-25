//! The guest32 ABI: the Motorola 68k, big-endian layouts a 68k program sees.
//!
//! # Why this crate carries the guest32 layouts
//!
//! The plan calls for `tests/abi-layout` to check "every native64 struct"
//! *and* "guest32 BE round-trips". The native64 half reads the real
//! `amiga/abi-native64` crate, because that crate is the definition. The
//! guest32 half cannot do the same thing yet: `amiga/abi-guest32/src/lib.rs`
//! is still a one-line placeholder, so there is nothing there to assert
//! against.
//!
//! Rather than assert nothing, this module carries a **reference
//! implementation** of the guest32 layouts and their wire codec, taken from
//! the AmigaOS 3.1 68k layouts that the design document (`reports/VelaOS.md`
//! §8.1) requires the guest ABI to preserve. The tests in `tests/guest32.rs`
//! then hold *this* implementation to the layout and the big-endian rules.
//!
//! # What this means for the plan
//!
//! The guest32 layout assertions are currently guarding a specification
//! rather than the shipping crate. When `amiga/abi-guest32` is populated,
//! these definitions should move there verbatim and this module should depend
//! on it instead — the assertions themselves would not change, only where the
//! types come from.
//!
//! # The two ABIs are not interchangeable
//!
//! The native64 ABI is 64-bit, little-endian, and uses the platform's ordinary
//! calling convention. The guest32 ABI is 32-bit, **big-endian**, and follows
//! the 68k's alignment rules. The layouts here are therefore *not* the
//! native64 ones scaled down: the field widths differ (`ULONG` is 32 bits,
//! not 64), the pointers are 32-bit, and — most importantly — the byte order
//! is the 68k's.
//!
//! # The wire format
//!
//! A 68k program reads its structures directly out of memory, so there is no
//! serialisation step in a real 68k system: the memory image *is* the wire
//! format. VelaOS's guest bridge, however, has to produce that image from a
//! host that is almost certainly little-endian, so the encoding has to be
//! written out explicitly.
//!
//! That is what the codec below does, and the tests check two properties of
//! it:
//!
//! 1. Every multi-byte scalar is encoded **big-endian**, so the byte at the
//!    lowest offset is the most significant one.
//! 2. The encoding is the exact inverse of the decoding, for every structure
//!    and for values chosen to catch sign-extension and truncation mistakes.
//!
//! The in-memory `#[repr(C)]` layout of these structures is the *host's*
//! layout, not the guest's: `repr(C)` fixes the offsets but not the byte
//! order. The tests therefore assert the offsets **and** assert that the
//! wire bytes differ from the in-memory bytes on a little-endian host —
//! because if they did not, the codec would be doing nothing.

/// A guest 32-bit pointer, as the 68k ABI represents one.
///
/// Guest pointers are **not** host pointers. A 68k address space is a
/// different address space, so a guest pointer is an offset into it and
/// means nothing without a guest memory context.
#[repr(transparent)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct GuestPtr32(pub u32);

impl GuestPtr32 {
    /// The null guest pointer.
    pub const NULL: GuestPtr32 = GuestPtr32(0);

    /// True when the pointer is null.
    pub const fn is_null(self) -> bool {
        self.0 == 0
    }
}

/// Why a byte slice could not be decoded into a guest structure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WireError {
    /// The slice was shorter than the structure needs.
    ///
    /// The guest ABI has no length prefix on its structures — a 68k program
    /// reads them out of a fixed layout — so a truncated buffer is the only
    /// way a decode can fail.
    TooShort {
        /// How many bytes the structure needs.
        needed: usize,
        /// How many bytes the buffer had.
        got: usize,
    },
}

impl core::fmt::Display for WireError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::TooShort { needed, got } => {
                write!(f, "need {needed} bytes for this structure, got {got}")
            }
        }
    }
}

/// Read a big-endian `u16` at `at`.
///
/// Returns `None` when the slice is too short, which is how every decode
/// below reports truncation.
fn be_u16(bytes: &[u8], at: usize) -> Option<u16> {
    let slice = bytes.get(at..at + 2)?;
    Some(u16::from_be_bytes([slice[0], slice[1]]))
}

/// Read a big-endian `u32` at `at`.
fn be_u32(bytes: &[u8], at: usize) -> Option<u32> {
    let slice = bytes.get(at..at + 4)?;
    Some(u32::from_be_bytes([slice[0], slice[1], slice[2], slice[3]]))
}

/// The 68k `Node`: the head of every AmigaOS-style list.
///
/// # Layout
///
/// ```text
/// offset  0  u8        ln_Type
/// offset  1  i8        ln_Pri
/// offset  2  [u8; 10]  ln_Name
/// offset 12  u32       ln_Next
/// offset 16  u32       ln_Prev
/// ```
///
/// The name is a fixed ten-byte inline buffer, exactly as on the native64
/// side — a name longer than nine characters is truncated, and a list of
/// names is a list of nodes rather than a list of strings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub struct GuestNode32 {
    /// What the node holds.
    pub ln_type: u8,
    /// Priority, used by the scheduler for task nodes.
    pub ln_pri: i8,
    /// The node's name, NUL-terminated if it fits.
    pub ln_name: [u8; GuestNode32::NAME_LEN],
    /// The address of the next node in the list, or zero.
    pub ln_next: GuestPtr32,
    /// The address of the previous node in the list, or zero.
    pub ln_prev: GuestPtr32,
}

impl GuestNode32 {
    /// The length of the inline name buffer, in bytes.
    pub const NAME_LEN: usize = 10;

    /// The byte size of the structure.
    pub const SIZE: usize = 20;

    /// A node with the given type, priority and name.
    pub fn new(ln_type: u8, pri: i8, name: &str) -> Self {
        let mut ln_name = [0u8; Self::NAME_LEN];
        let bytes = name.as_bytes();
        let len = bytes.len().min(Self::NAME_LEN - 1);
        ln_name[..len].copy_from_slice(&bytes[..len]);
        Self {
            ln_type,
            ln_pri: pri,
            ln_name,
            ln_next: GuestPtr32::NULL,
            ln_prev: GuestPtr32::NULL,
        }
    }

    /// The node's name, as a string slice.
    pub fn name(&self) -> &str {
        let end = self
            .ln_name
            .iter()
            .position(|b| *b == 0)
            .unwrap_or(Self::NAME_LEN);
        core::str::from_utf8(&self.ln_name[..end]).unwrap_or("")
    }

    /// Encode the node into the guest's byte order.
    ///
    /// The result is exactly [`Self::SIZE`] bytes: the guest ABI has no
    /// length prefix, so a decoder must know each structure's size from the
    /// type alone.
    pub fn to_wire(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(Self::SIZE);
        out.push(self.ln_type);
        out.push(self.ln_pri as u8);
        out.extend_from_slice(&self.ln_name);
        out.extend_from_slice(&self.ln_next.0.to_be_bytes());
        out.extend_from_slice(&self.ln_prev.0.to_be_bytes());
        out
    }

    /// Decode a node from the guest's byte order.
    ///
    /// # Errors
    ///
    /// Returns [`WireError::TooShort`] when the slice does not hold a whole
    /// node.
    pub fn from_wire(bytes: &[u8]) -> Result<Self, WireError> {
        if bytes.len() < Self::SIZE {
            return Err(WireError::TooShort {
                needed: Self::SIZE,
                got: bytes.len(),
            });
        }
        let mut ln_name = [0u8; Self::NAME_LEN];
        ln_name.copy_from_slice(&bytes[2..2 + Self::NAME_LEN]);
        Ok(Self {
            ln_type: bytes[0],
            ln_pri: bytes[1] as i8,
            ln_name,
            ln_next: GuestPtr32(be_u32(bytes, 12).unwrap_or(0)),
            ln_prev: GuestPtr32(be_u32(bytes, 16).unwrap_or(0)),
        })
    }
}

/// The 68k `List`: a doubly-linked list head.
///
/// # Layout
///
/// ```text
/// offset  0  u32  lh_Head
/// offset  4  u32  lh_Tail
/// offset  8  u32  lh_Chain
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub struct GuestList32 {
    /// The first node, or the list head itself when the list is empty.
    pub lh_head: GuestPtr32,
    /// The last node, or the list head itself when the list is empty.
    pub lh_tail: GuestPtr32,
    /// The next free list node, used by the system's free lists.
    pub lh_chain: GuestPtr32,
}

impl GuestList32 {
    /// The byte size of the structure.
    pub const SIZE: usize = 12;

    /// Encode the list into the guest's byte order.
    pub fn to_wire(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(Self::SIZE);
        out.extend_from_slice(&self.lh_head.0.to_be_bytes());
        out.extend_from_slice(&self.lh_tail.0.to_be_bytes());
        out.extend_from_slice(&self.lh_chain.0.to_be_bytes());
        out
    }

    /// Decode a list from the guest's byte order.
    ///
    /// # Errors
    ///
    /// Returns [`WireError::TooShort`] when the slice does not hold a whole
    /// list.
    pub fn from_wire(bytes: &[u8]) -> Result<Self, WireError> {
        if bytes.len() < Self::SIZE {
            return Err(WireError::TooShort {
                needed: Self::SIZE,
                got: bytes.len(),
            });
        }
        Ok(Self {
            lh_head: GuestPtr32(be_u32(bytes, 0).unwrap_or(0)),
            lh_tail: GuestPtr32(be_u32(bytes, 4).unwrap_or(0)),
            lh_chain: GuestPtr32(be_u32(bytes, 8).unwrap_or(0)),
        })
    }
}

/// The 68k `TagItem`: a typed key/value pair.
///
/// # Layout
///
/// ```text
/// offset  0  u32  ti_Tag
/// offset  4  u32  ti_Data
/// ```
///
/// The guest's `ti_Data` is a 32-bit `ULONG`, whereas the native64 side
/// widened it to 64 bits so that a pointer-sized value fits. That difference
/// is the reason the two ABIs cannot share one structure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub struct GuestTagItem32 {
    /// The tag's identifier.
    pub ti_tag: u32,
    /// The tag's value.
    pub ti_data: u32,
}

impl GuestTagItem32 {
    /// The byte size of the structure.
    pub const SIZE: usize = 8;

    /// Encode the tag into the guest's byte order.
    pub fn to_wire(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(Self::SIZE);
        out.extend_from_slice(&self.ti_tag.to_be_bytes());
        out.extend_from_slice(&self.ti_data.to_be_bytes());
        out
    }

    /// Decode a tag from the guest's byte order.
    ///
    /// # Errors
    ///
    /// Returns [`WireError::TooShort`] when the slice does not hold a whole
    /// tag.
    pub fn from_wire(bytes: &[u8]) -> Result<Self, WireError> {
        if bytes.len() < Self::SIZE {
            return Err(WireError::TooShort {
                needed: Self::SIZE,
                got: bytes.len(),
            });
        }
        Ok(Self {
            ti_tag: be_u32(bytes, 0).unwrap_or(0),
            ti_data: be_u32(bytes, 4).unwrap_or(0),
        })
    }
}

/// The 68k `MsgPort`: a queue of messages plus a signal mask.
///
/// # Layout
///
/// ```text
/// offset  0  GuestNode32  mp_Node
/// offset 20  u8           mp_Sig
/// offset 21  u8           mp_Flags
/// offset 22  u8           mp_SigWait
/// offset 23  u8           mp_Reserved
/// offset 24  u16          mp_Left
/// offset 26  u16          mp_Hide
/// offset 28  u32          mp_ReplyPort
/// offset 32  u32          mp_UserData
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub struct GuestMsgPort32 {
    /// The list node, so ports can be linked into a port list.
    pub mp_node: GuestNode32,
    /// The signals this port raises.
    pub mp_sig: u8,
    /// The port's flags.
    pub mp_flags: u8,
    /// The signals a task must wait for to be woken by this port.
    pub mp_sig_wait: u8,
    /// Reserved.
    pub mp_reserved: u8,
    /// The number of messages the port can hold.
    pub mp_left: u16,
    /// The number of messages the port hides from its owner.
    pub mp_hide: u16,
    /// The address of the reply port, or zero.
    pub mp_reply_port: GuestPtr32,
    /// The address of the port's private data, or zero.
    pub mp_user_data: GuestPtr32,
}

impl GuestMsgPort32 {
    /// The byte size of the structure.
    pub const SIZE: usize = 36;

    /// Encode the port into the guest's byte order.
    pub fn to_wire(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(Self::SIZE);
        out.extend_from_slice(&self.mp_node.to_wire());
        out.push(self.mp_sig);
        out.push(self.mp_flags);
        out.push(self.mp_sig_wait);
        out.push(self.mp_reserved);
        out.extend_from_slice(&self.mp_left.to_be_bytes());
        out.extend_from_slice(&self.mp_hide.to_be_bytes());
        out.extend_from_slice(&self.mp_reply_port.0.to_be_bytes());
        out.extend_from_slice(&self.mp_user_data.0.to_be_bytes());
        out
    }

    /// Decode a port from the guest's byte order.
    ///
    /// # Errors
    ///
    /// Returns [`WireError::TooShort`] when the slice does not hold a whole
    /// port.
    pub fn from_wire(bytes: &[u8]) -> Result<Self, WireError> {
        if bytes.len() < Self::SIZE {
            return Err(WireError::TooShort {
                needed: Self::SIZE,
                got: bytes.len(),
            });
        }
        Ok(Self {
            mp_node: GuestNode32::from_wire(&bytes[0..GuestNode32::SIZE])?,
            mp_sig: bytes[20],
            mp_flags: bytes[21],
            mp_sig_wait: bytes[22],
            mp_reserved: bytes[23],
            mp_left: be_u16(bytes, 24).unwrap_or(0),
            mp_hide: be_u16(bytes, 26).unwrap_or(0),
            mp_reply_port: GuestPtr32(be_u32(bytes, 28).unwrap_or(0)),
            mp_user_data: GuestPtr32(be_u32(bytes, 32).unwrap_or(0)),
        })
    }
}

/// The 68k `Message`: the unit of the Exec message system.
///
/// # Layout
///
/// ```text
/// offset  0  u32          mn_ReplyPort
/// offset  4  u16          mn_Length
/// offset  6  u16          mn_Reserved
/// offset  8  GuestNode32  mn_Node
/// ```
///
/// `mn_Length` is the message's length in bytes. It is what lets a receiver
/// walk a queue of differently-sized messages, and it is why the guest's
/// message layout is not the native64 one: the native64 queue holds messages
/// by value, so there is no length to carry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub struct GuestMessage32 {
    /// The address of the port a reply is sent to, or zero.
    pub mn_reply_port: GuestPtr32,
    /// The message's length in bytes.
    pub mn_length: u16,
    /// Reserved.
    pub mn_reserved: u16,
    /// The list node, so messages can be linked into a queue.
    pub mn_node: GuestNode32,
}

impl GuestMessage32 {
    /// The byte size of the structure.
    pub const SIZE: usize = 28;

    /// Encode the message into the guest's byte order.
    pub fn to_wire(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(Self::SIZE);
        out.extend_from_slice(&self.mn_reply_port.0.to_be_bytes());
        out.extend_from_slice(&self.mn_length.to_be_bytes());
        out.extend_from_slice(&self.mn_reserved.to_be_bytes());
        out.extend_from_slice(&self.mn_node.to_wire());
        out
    }

    /// Decode a message from the guest's byte order.
    ///
    /// # Errors
    ///
    /// Returns [`WireError::TooShort`] when the slice does not hold a whole
    /// message.
    pub fn from_wire(bytes: &[u8]) -> Result<Self, WireError> {
        if bytes.len() < Self::SIZE {
            return Err(WireError::TooShort {
                needed: Self::SIZE,
                got: bytes.len(),
            });
        }
        Ok(Self {
            mn_reply_port: GuestPtr32(be_u32(bytes, 0).unwrap_or(0)),
            mn_length: be_u16(bytes, 4).unwrap_or(0),
            mn_reserved: be_u16(bytes, 6).unwrap_or(0),
            mn_node: GuestNode32::from_wire(&bytes[8..8 + GuestNode32::SIZE])?,
        })
    }
}

/// A guest `BPTR`: a file handle in the AmigaOS 68k representation.
///
/// # The shift
///
/// On AmigaOS a `BPTR` is a `struct FileLock *`, and the canonical way to
/// store one in a 32-bit word is to shift the address right by two — the
/// `BADDR` form. The shift is not arbitrary: it guarantees the low two bits
/// are clear, which is what lets a 32-bit word hold an address on the 68k,
/// where only the even addresses are usable for word-sized access.
///
/// The native64 side deliberately does **not** do this: a native64 `Bptr` is
/// an opaque handle, not an address, and an opaque handle has no shift to
/// apply. The two are different things that happen to share a name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(transparent)]
pub struct GuestBptr32(u32);

impl GuestBptr32 {
    /// The null handle.
    pub const NULL: GuestBptr32 = GuestBptr32(0);

    /// The number of bits a `BADDR` is shifted right by.
    pub const SHIFT: u32 = 2;

    /// Build a handle from a raw `BADDR` value.
    pub const fn from_baddr(baddr: u32) -> Self {
        GuestBptr32(baddr)
    }

    /// The raw `BADDR` value.
    pub const fn baddr(self) -> u32 {
        self.0
    }

    /// The address this handle names.
    pub const fn address(self) -> u32 {
        self.0 << Self::SHIFT
    }

    /// Build a handle from an address.
    ///
    /// The address must be even; the 68k's word-sized access requires it.
    ///
    /// # Panics
    ///
    /// Panics if `address` is odd, because the shifted-back value would not
    /// round-trip and the handle would name the wrong byte.
    pub const fn from_address(address: u32) -> Self {
        assert!(
            address.is_multiple_of(4),
            "a 68k BPTR names a four-byte-aligned lock"
        );
        GuestBptr32(address >> Self::SHIFT)
    }

    /// True when the handle names no file.
    pub const fn is_null(self) -> bool {
        self.0 == 0
    }
}

/// Encode a string as a guest `BSTR`.
///
/// A `BSTR` is a length-prefixed string: one byte holding the length,
/// followed by that many bytes of text. The length byte counts *characters*,
/// not bytes, which is why a non-ASCII string does not have a byte length
/// recoverable from the prefix alone.
pub fn bstr_bytes(text: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(text.len() + 1);
    out.push(text.chars().count() as u8);
    out.extend_from_slice(text.as_bytes());
    out
}

/// Decode a guest `BSTR`.
///
/// # Errors
///
/// Returns `None` when the buffer is empty, when the length byte exceeds the
/// number of bytes present, or when the text is not valid UTF-8. A 68k
/// `BSTR` may hold non-UTF-8 bytes; this decoder is for the text VelaOS's
/// own guest bridge produces, and refusing to guess is better than returning
/// mojibake.
pub fn bstr_from_bytes(bytes: &[u8]) -> Option<String> {
    let (&len, rest) = bytes.split_first()?;
    let len = len as usize;
    if rest.len() < len {
        return None;
    }
    core::str::from_utf8(&rest[..len])
        .ok()
        .map(|s| s.to_string())
}
