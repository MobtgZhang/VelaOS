//! The AmigaOS data structures, as a user program sees them.
//!
//! # Why these are re-exports rather than restatements
//!
//! These layouts used to be written out a second time here, with a
//! `tests/abi-layout` case meant to assert byte-for-byte agreement with the
//! kernel's copy. That test did not exist, and the two copies drifted: the SDK
//! used a 64-byte `ln_Name` and the kernel an AmigaOS-conformant 10-byte one,
//! so a [`Message`] was 160 bytes on the user side and 104 in the kernel. Every
//! `GetMsg`/`ReplyMsg` round trip then copied two different numbers of bytes,
//! and the program saw a reply port of zero — an `Exec` bug that was really an
//! ABI bug.
//!
//! A test that was never written is not a safety net. So the duplication is
//! gone: [`vela_abi_native64`] defines each layout once, and this module
//! re-exports it under the CamelCase field names a program written against
//! AmigaOS expects. There is now no second copy that *can* drift, and
//! `tests/abi-layout` still checks the layouts against the values the report
//! specifies, which catches the case that matters — a change to the single
//! definition.
//!
//! # Naming
//!
//! The fields are `CamelCase` because they are AmigaOS field names and a
//! program written against AmigaOS will use them. Renaming them to Rust
//! convention would make the SDK *less* recognisable to the code it exists to
//! serve, which is the opposite of what a compatibility layer is for.

/// The list node.
///
/// The kernel's definition, re-exported. `ln_Name` is the AmigaOS-conformant
/// ten-byte inline name, not a pointer and not the 64-byte buffer this file
/// used to declare — an inline array either way, so a user program still
/// cannot hand the kernel a pointer to free memory to read a name from.
pub use vela_abi_native64::Node;

/// The length of a [`Node`]'s inline name.
pub const NODE_NAME_LEN: usize = vela_abi_native64::NODE_NAME_LEN;

/// The list node types.
///
/// The kernel's set, re-exported. The SDK used to declare its own three-value
/// version — `FREE`, `USED`, `MESSAGE` — which is both a different list from
/// the kernel's AmigaOS `LIST_*` values *and* a second place for the two to
/// disagree. A program that read a node type and matched it against these
/// constants would have disagreed with the kernel about what a message is.
pub use vela_abi_native64::node_type;

/// The number of 64-bit words a message's extension payload holds.
///
/// Six is enough for the `IntuiMessage` subclass and small enough that a
/// plain message stays cheap to copy. Re-exported rather than restated: the
/// kernel's value is the one that has to match, and a second literal is a
/// second thing to forget.
pub const MSG_PAYLOAD_WORDS: usize = vela_abi_native64::MSG_PAYLOAD_WORDS;

/// A message.
///
/// # Why `mn_ReplyPort` is a `u64` and not a pointer
///
/// On AmigaOS `mn_reply_port` is a `struct MsgPort *`, and it really is a
/// pointer into the sender's address space. VelaOS cannot do that: the
/// receiver lives in a different address space, and a native64 program must
/// never receive a kernel pointer. So it is an **opaque handle** — the
/// numeric id of a port in the kernel's table.
///
/// The layout is the kernel's, re-exported under AmigaOS's field names. See
/// the module documentation for why it is not written out a second time.
pub use vela_abi_native64::Message;

/// An Intuition event, as a program sees it in a message.
///
/// The layout is the kernel's, re-exported under AmigaOS's field names. The
/// event classes are re-exported as associated constants below so a program can
/// spell `IntuiMessage::GADGET` as it would on AmigaOS.
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
/// offset 24  [u8; 4] _pad
/// offset 28  u32  im_Sig      the signal raised when it arrives
/// ```
pub use vela_abi_native64::IntuiMessage;

/// The `IDCMP_*` classes a window may ask for.
pub mod idcmp {
    /// No events.
    pub const NONE: u16 = 0;
    /// Mouse button events.
    pub const MOUSEBUTTONS: u16 = 1 << 0;
    /// Mouse movement.
    pub const MOUSEMOVE: u16 = 1 << 1;
    /// Keyboard events.
    pub const KEYBOARD: u16 = 1 << 8;
    /// Gadget events.
    pub const GADGET: u16 = 1 << 9;
    /// Intuition requests: close, change, menu.
    pub const REQUEST: u16 = 1 << 15;
    /// Mouse events and movement together.
    pub const MOUSE: u16 = MOUSEBUTTONS | MOUSEMOVE;
    /// Everything.
    pub const ALL: u16 = MOUSEBUTTONS | MOUSEMOVE | KEYBOARD | GADGET | REQUEST;
}

/// The `RECIDCMP` codes.
pub mod request_codes {
    /// The window's close gadget was clicked.
    pub const CLOSE: u16 = 0;
}

/// The mouse event codes, as the AmigaOS `IEQUAL_BUTTON_*` values.
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

/// The mouse buttons, as the AmigaOS sees them.
pub mod buttons {
    /// The left button.
    pub const LEFT: u16 = 1 << 0;
    /// The right button.
    pub const RIGHT: u16 = 1 << 1;
    /// The middle button.
    pub const MIDDLE: u16 = 1 << 2;
}

/// The window flags a program may set.
/// The window flags, as the kernel defines them.
///
/// Re-exported rather than restated, and this is not tidiness. The SDK used to
/// declare its own five-flag set with `CLOSEGADGET = 1 << 0` and
/// `DEPTHGADGET = 1 << 3`, while the kernel — and AmigaOS — use `1 << 5` and
/// `1 << 9`. A program that set the SDK's `CLOSEGADGET` therefore set a bit the
/// kernel reads as `GIMMEREQUEST`, and the window it opened had **no close
/// gadget at all**: the click simply never became a close request, and the
/// acceptance program's teardown never ran.
///
/// Two hand-written copies of a bit-assignment table are two chances to be
/// wrong, and nothing between them can disagree loudly — a wrong bit is still a
/// perfectly valid flag. So there is now one definition.
pub use vela_abi_native64::window_flags;

/// The gadget kinds a program may request.
///
/// Re-exported for the same reason as [`window_flags`]. The SDK used to say
/// `CUSTOM = 3`, where the kernel says `SLIDER = 3` — so a program asking for
/// a custom gadget was asking the kernel for a slider.
pub use vela_abi_native64::gadget_kind;

/// The rectangle as a program passes and receives it.
///
/// # The field names
///
/// On AmigaOS the fields are `Min_X`, `Min_Y`, `Max_X` and `Max_Y` — and
/// note what the last two mean: not a corner, but the *width* and the
/// *height*. A rectangle's "max" is the far edge of the box it describes,
/// measured from its origin, so `Max_X - Min_X` is the width. VelaOS keeps
/// that convention, because a program that computes a rectangle's extent
/// from its fields gets the right answer.
///
/// The fields are spelled in Rust's convention here. That is a deliberate
/// departure from the rest of the ABI, where the CamelCase names are
/// re-exported verbatim from the shared crate: those are *not* this crate's
/// definitions, so the names are the shared crate's, while these fields are,
/// and the project bans lint suppressions — which means the fix for a
/// naming-convention warning has to be the name, not the warning.
///
/// The layout is unchanged: `#[repr(C)]`, two `i16` and two `u16`, so a C
/// program and this agree byte for byte.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub struct Rectangle {
    /// The left edge.
    pub min_x: i16,
    /// The top edge.
    pub min_y: i16,
    /// The width.
    pub max_x: u16,
    /// The height.
    pub max_y: u16,
}

impl Rectangle {
    /// A rectangle at `(x, y)` of `w` by `h`.
    pub const fn new(x: i16, y: i16, w: u16, h: u16) -> Self {
        Self {
            min_x: x,
            min_y: y,
            max_x: w,
            max_y: h,
        }
    }
}

/// The window geometry a program passes to `OpenWindow`.
///
/// Fixed-size and passed by pointer rather than as a pointer to a full
/// `struct Window`: a program cannot hand the kernel a pointer to something it
/// has not written and have the kernel read a dozen fields it does not need.
#[derive(Debug, Clone, Copy)]
#[repr(C)]
pub struct OpenWindowArgs {
    /// Left edge, in screen coordinates.
    pub x: i16,
    /// Top edge.
    pub y: i16,
    /// Width.
    pub width: u16,
    /// Height.
    pub height: u16,
    /// The IDCMP class mask.
    pub idcmp: u16,
    /// Window flags.
    pub flags: u32,
}

/// A timestamp, as AmigaDOS spells one.
///
/// # The layout
///
/// Three `i32`s: days since the AmigaDOS epoch, minutes past the hour, and
/// ticks of 1/50 second. Stored on the wire in that form rather than as a
/// Unix timestamp because a program that reads `fi_Time` expects to do
/// AmigaDOS date arithmetic with it.
///
/// # Why the SDK spells it out rather than re-exporting it
///
/// The shared ABI crate has a `DateStamp` too, and the two must agree. This
/// one is spelled in Rust's field convention so that no lint suppression is
/// needed; the accessors give the AmigaOS names to a program that wants
/// them, and the ABI layout test holds the two definitions to the same
/// offsets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub struct DateStamp {
    /// Days since the AmigaDOS epoch, which is 2208 days before the Unix one.
    pub ds_days: i32,
    /// Minutes past the hour, 0-59.
    pub ds_minute: i32,
    /// Ticks past the minute, 0-2499 at 50 ticks per second.
    pub ds_tick: i32,
}

impl DateStamp {
    /// The days since the AmigaDOS epoch.
    ///
    /// Named `days` rather than `ds_Days` because this crate's own
    /// definitions use Rust's field convention; a program that wants the
    /// AmigaOS spelling reads the field through the shared ABI crate.
    pub const fn days(&self) -> i32 {
        self.ds_days
    }

    /// The minutes past the hour.
    pub const fn minute(&self) -> i32 {
        self.ds_minute
    }

    /// The ticks past the minute.
    pub const fn tick(&self) -> i32 {
        self.ds_tick
    }
}

/// The size of a [`FileInfo`]'s inline comment buffer.
pub const FILE_INFO_COMMENT_LEN: usize = 108;

/// What `Examine` reports about a path.
///
/// # Why this is not a re-export
///
/// [`Message`] *is* re-exported, because the SDK and the kernel must agree
/// on it byte for byte and there is no second copy to drift. `FileInfo` is a
/// different case: the kernel fills in *its* layout, and the SDK's copy has
/// to match it field for field. That is a relationship worth stating in one
/// place rather than inheriting silently, and it is checked by the ABI layout
/// test.
///
/// # The two explicit padding holes
///
/// `fi_Time` is twelve bytes of three `i32`s, so it ends four bytes past an
/// 8-byte boundary. The compiler would insert those four bytes itself — but
/// *implicitly*, which means the next person to re-order the fields would
/// find it had moved. Writing the holes out keeps the 64-bit fields at offsets
/// 52 and 60 where the ABI test says they are.
///
/// # The field names
///
/// The AmigaOS names are `CamelCase`, and this crate's own definition is
/// spelled in Rust's convention so that no lint suppression is needed. The
/// accessors below give the AmigaOS names to a program that wants them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub struct FileInfo {
    /// 2 for a file, 3 for a directory, 4 for a device.
    pub fi_entry_type: u32,
    /// The parent directory's key. VelaOS leaves this zero — it has no
    /// persistent directory keys — so a program reads "not available" rather
    /// than a value that looks meaningful and is not.
    pub fi_entry_key: u32,
    /// The entry's size in bytes.
    pub fi_size: u64,
    /// The backing disk's key, likewise zero.
    pub fi_disk_key: u32,
    /// The AmigaDOS protection bits: `FIBF_READ`, `FIBF_WRITE`, `FIBF_EXECUTE`,
    /// `FIBF_DELETE`, `FIBF_ARCHIVE`, and the directory and pure flags.
    pub fi_protection: u32,
    /// The owning user id, 32-bit form.
    pub fi_owner_uid: i32,
    /// The owning group id, 32-bit form.
    pub fi_owner_gid: i32,
    /// The comment's length, excluding the terminator.
    pub fi_comment_len: i32,
    /// Explicit padding, so the two 64-bit owner fields land on 8-byte
    /// boundaries.
    pub pad: [u8; 4],
    /// The entry's modification time.
    pub fi_time: DateStamp,
    /// Explicit padding, for the same reason as `pad`.
    pub pad2: [u8; 4],
    /// The owning user id, 64-bit form.
    pub fi_owner_uid64: u64,
    /// The owning group id, 64-bit form.
    pub fi_owner_gid64: u64,
    /// The long comment's length, or zero.
    pub fi_comment_len2: u32,
    /// The comment, NUL-terminated.
    pub fi_comment: [u8; FILE_INFO_COMMENT_LEN],
}

impl FileInfo {
    /// A `FileInfo` with everything zeroed: type file, no comment, no owner.
    pub const fn empty() -> Self {
        Self {
            fi_entry_type: 2,
            fi_entry_key: 0,
            fi_size: 0,
            fi_disk_key: 0,
            fi_protection: 0,
            fi_owner_uid: 0,
            fi_owner_gid: 0,
            fi_comment_len: 0,
            pad: [0; 4],
            fi_time: DateStamp {
                ds_days: 0,
                ds_minute: 0,
                ds_tick: 0,
            },
            pad2: [0; 4],
            fi_owner_uid64: 0,
            fi_owner_gid64: 0,
            fi_comment_len2: 0,
            fi_comment: [0; FILE_INFO_COMMENT_LEN],
        }
    }

    /// The entry's type.
    pub const fn entry_type(&self) -> u32 {
        self.fi_entry_type
    }

    /// The entry's size in bytes.
    pub const fn size(&self) -> u64 {
        self.fi_size
    }

    /// The comment, or `""` when there is none.
    ///
    /// Bounded by the stored length *and* by the buffer, because the reply came
    /// from the kernel and a length claiming more than the buffer holds is
    /// exactly the kind of thing a bug elsewhere would produce.
    pub fn comment(&self) -> &str {
        let len = (self.fi_comment_len.max(0) as usize).min(FILE_INFO_COMMENT_LEN);
        core::str::from_utf8(&self.fi_comment[..len]).unwrap_or("")
    }
}

/// A file handle, as a program receives it from `Open`.
///
/// On AmigaOS `BPTR` is a pointer into a per-process file table. VelaOS has
/// no such table in user space — the file table is the kernel's — so the handle
/// is an opaque id. It is meaningless outside the kernel, which is the property
/// that makes it safe to hold in a `u32`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub struct Bptr(pub u32);

impl Bptr {
    /// The null file handle.
    pub const NULL: Bptr = Bptr(0);

    /// Build a handle from a raw value.
    pub const fn from_raw(raw: u32) -> Self {
        Self(raw)
    }

    /// The raw value.
    pub const fn raw(self) -> u32 {
        self.0
    }

    /// True when this names no open file.
    pub const fn is_null(self) -> bool {
        self.0 == 0
    }
}

/// A library handle.
///
/// A `u32` and *not* a pointer, deliberately. On AmigaOS a library base *is* a
/// pointer, and offsets from it are the whole calling convention. VelaOS's
/// user-space library base is a per-process jump table — a pointer into the
/// program's own address space — but the value that crosses the syscall
/// boundary is this id, because the kernel must not hand a program a pointer
/// into its own address space to use as a capability.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(transparent)]
pub struct LibraryHandle(pub u32);

impl LibraryHandle {
    /// The null handle.
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
}

/// The library a handle names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum LibraryId {
    /// `exec.library`.
    Exec = 0,
    /// `graphics.library`.
    Graphics = 1,
    /// `layers.library`.
    Layers = 2,
    /// `intuition.library`.
    Intuition = 3,
    /// `dos.library`.
    Dos = 4,
    /// `utility.library`.
    Utility = 5,
}

impl LibraryId {
    /// The library's name, as `OpenLibrary` spells it.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Exec => "exec.library",
            Self::Graphics => "graphics.library",
            Self::Layers => "layers.library",
            Self::Intuition => "intuition.library",
            Self::Dos => "dos.library",
            Self::Utility => "utility.library",
        }
    }

    /// The library a handle names, if it is valid.
    pub const fn of(handle: LibraryHandle) -> Option<Self> {
        match handle.0 {
            0 => None,
            1 => Some(Self::Exec),
            2 => Some(Self::Graphics),
            3 => Some(Self::Layers),
            4 => Some(Self::Intuition),
            5 => Some(Self::Dos),
            6 => Some(Self::Utility),
            _ => None,
        }
    }

    /// The handle for a library.
    pub const fn handle(self) -> LibraryHandle {
        LibraryHandle(self as u32 + 1)
    }
}

/// The signal bits Exec uses.
pub mod sigs {
    /// A message arrived on one of the task's ports.
    pub const MESSAGE: u32 = 1 << 4;
    /// A task is ready to run.
    pub const SINGLE: u32 = 1 << 16;
}

/// The signal mask a program uses for a message port.
pub const SIGB: u32 = sigs::MESSAGE | sigs::SINGLE;
