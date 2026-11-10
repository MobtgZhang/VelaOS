//! The VelaOS API conformance matrix and the machinery around it.
//!
//! # What this crate is
//!
//! The design document (`reports/VelaOS.md` §7.6) asks for a *registry
//! template* for every AmigaOS-style API: a name, the library it belongs to,
//! a minimum version, a status for each ABI family, and the tests that
//! cover it. This crate is that registry, expressed as data, plus the
//! generator that turns it into the conformance documents under
//! `docs/amiga-api/`.
//!
//! # The status taxonomy
//!
//! Four statuses, and the distinction between them is the whole point:
//!
//! | status | meaning |
//! |---|---|
//! | `not-started` | the API is not reachable at all — no syscall, no service method |
//! | `stub` | the API exists but returns a fixed placeholder |
//! | `implemented` | the API exists and does something plausible, but no test asserts the *behaviour* |
//! | `verified` | a test in this crate asserts the documented behaviour of the API |
//!
//! The document's own rule is the one that matters: **a stub is not
//! compatible**. A `stub` that returned success would be indistinguishable
//! from a working call, which is why `not-started` APIs report
//! `NOT_IMPLEMENTED` through the syscall dispatcher rather than pretending.
//!
//! # Why the matrix is data and not a test
//!
//! The matrix is the *input* to the test suite, not its output. A test that
//! read a status out of the same place it wrote it would assert nothing. So
//! the matrix is a `const` table, the behaviour tests in `tests/` assert
//! against the *kernel*, and a separate consistency test proves that every
//! entry marked `verified` really does have a behaviour test — by naming it.
//!
//! # Generating the documents
//!
//! [`render_library_matrix`] produces the Markdown for one library, and
//! [`render_all`] produces the whole set. The documents under
//! `docs/amiga-api/` are generated from this table and are checked by
//! `tests/documents.rs`, which fails if the checked-in file and the table
//! have drifted apart.
//!
//! Setting `VELA_UPDATE_CONFORMANCE_DOCS=1` makes that test *write* the
//! documents instead of comparing them, which is how they are regenerated.

use std::collections::BTreeMap;

/// The libraries VelaOS v0.1.0 exposes.
///
/// The set matches `vela_abi_native64::LibraryId::ALL` and the syscall
/// groups in `vela_kernel_syscall::numbers`.
pub const LIBRARIES: &[&str] = &[
    "exec.library",
    "dos.library",
    "graphics.library",
    "layers.library",
    "intuition.library",
    "utility.library",
    "icon.library",
    "workbench.library",
];

/// How completely an API is implemented.
///
/// The four values are ordered by how much they promise: `NotStarted` promises
/// nothing, `Stub` promises only that the call exists, `Implemented` promises
/// that the call does the obvious thing, and `Verified` promises that a test
/// holds it to that promise.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Status {
    /// The API is not reachable: no syscall slot, no service method.
    NotStarted,
    /// The API exists but returns a fixed placeholder.
    Stub,
    /// The API exists and returns a plausible value, but no test asserts the
    /// behaviour.
    Implemented,
    /// A test in this crate asserts the documented behaviour.
    Verified,
}

impl Status {
    /// The status's spelling in the registry and in the generated documents.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NotStarted => "not-started",
            Self::Stub => "stub",
            Self::Implemented => "implemented",
            Self::Verified => "verified",
        }
    }

    /// True when a behaviour test holds this API to its promise.
    pub const fn is_verified(self) -> bool {
        matches!(self, Self::Verified)
    }

    /// Parse a status from its registry spelling.
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "not-started" => Some(Self::NotStarted),
            "stub" => Some(Self::Stub),
            "implemented" => Some(Self::Implemented),
            "verified" => Some(Self::Verified),
            _ => None,
        }
    }
}

/// One API entry in the registry.
///
/// The shape follows the design document's §7.6 template: a name, the
/// library it belongs to, the syscall that reaches it, the status, and a note
/// explaining anything a reader of the generated table could not infer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ApiEntry {
    /// The library the API belongs to, as in `exec.library`.
    pub library: &'static str,
    /// The API's name, in AmigaOS spelling.
    pub name: &'static str,
    /// The syscall number that reaches the API, or `None` when there is none.
    pub syscall: Option<u64>,
    /// How completely the API is implemented.
    pub status: Status,
    /// The name of the behaviour test that holds a `verified` API to its
    /// promise, or `None` when no such test exists.
    pub verified_by: Option<&'static str>,
    /// A free-form note, rendered into the generated document.
    pub notes: &'static str,
}

/// The registry: every API entry the matrix knows about.
///
/// The table is ordered by library and then by syscall number, so the
/// generated documents come out in the same order every time.
pub const MATRIX: &[ApiEntry] = &[
    // --- exec.library ----------------------------------------------------
    ApiEntry {
        library: "exec.library",
        name: "CreateMsgPort",
        syscall: Some(0),
        status: Status::Verified,
        verified_by: Some("exec_port_lifecycle_is_verified"),
        notes: "Returns a non-zero handle; deleting it makes the handle invalid.",
    },
    ApiEntry {
        library: "exec.library",
        name: "DeleteMsgPort",
        syscall: Some(1),
        status: Status::Verified,
        verified_by: Some("exec_port_lifecycle_is_verified"),
        notes: "Deleting a port that does not exist reports `NO_OBJECT`.",
    },
    ApiEntry {
        library: "exec.library",
        name: "PutMsg",
        syscall: Some(2),
        status: Status::Verified,
        verified_by: Some("exec_message_queue_is_fifo"),
        notes: "Messages are enqueued in order; a bad message pointer is refused.",
    },
    ApiEntry {
        library: "exec.library",
        name: "GetMsg",
        syscall: Some(3),
        status: Status::Verified,
        verified_by: Some("exec_message_queue_is_fifo"),
        notes: "A failed copy-out must not consume the message.",
    },
    ApiEntry {
        library: "exec.library",
        name: "ReplyMsg",
        syscall: Some(4),
        status: Status::Verified,
        verified_by: Some("exec_reply_goes_to_the_reply_port"),
        notes: "A reply with no reply port is refused with `NO_REPLY_PORT`.",
    },
    ApiEntry {
        library: "exec.library",
        name: "WaitPort",
        syscall: Some(5),
        status: Status::Implemented,
        verified_by: None,
        notes: "Returns 0 with a message, 1 when the wait ended without one.",
    },
    ApiEntry {
        library: "exec.library",
        name: "Wait",
        syscall: Some(6),
        status: Status::Implemented,
        verified_by: None,
        notes: "Returns the satisfied mask, or `u64::MAX` while still blocked.",
    },
    ApiEntry {
        library: "exec.library",
        name: "SetSignal",
        syscall: Some(7),
        status: Status::Implemented,
        verified_by: None,
        notes: "Returns the task's new signal set.",
    },
    ApiEntry {
        library: "exec.library",
        name: "SetExcept",
        syscall: Some(8),
        status: Status::Implemented,
        verified_by: None,
        notes: "Returns the task's new signal set.",
    },
    ApiEntry {
        library: "exec.library",
        name: "Signal",
        syscall: Some(9),
        status: Status::Implemented,
        verified_by: None,
        notes: "Raises bits in another task's signal set.",
    },
    ApiEntry {
        library: "exec.library",
        name: "OpenLibrary",
        syscall: Some(10),
        status: Status::Verified,
        verified_by: Some("exec_library_open_and_close"),
        notes: "An unknown library yields a null handle, not an error.",
    },
    ApiEntry {
        library: "exec.library",
        name: "CloseLibrary",
        syscall: Some(11),
        status: Status::Verified,
        verified_by: Some("exec_library_open_and_close"),
        notes: "Closing a handle that was never opened reports `NO_OBJECT`.",
    },
    ApiEntry {
        library: "exec.library",
        name: "Forbid",
        syscall: Some(12),
        status: Status::Verified,
        verified_by: Some("exec_forbid_and_permit_are_balanced"),
        notes: "Returns the new depth of the classic critical section.",
    },
    ApiEntry {
        library: "exec.library",
        name: "Permit",
        syscall: Some(13),
        status: Status::Verified,
        verified_by: Some("exec_forbid_and_permit_are_balanced"),
        notes: "Returns the remaining depth.",
    },
    ApiEntry {
        library: "exec.library",
        name: "Yield",
        syscall: Some(14),
        status: Status::Verified,
        verified_by: Some("exec_yield_requeues_the_task_after_its_body"),
        notes: "Records the request; the scheduler requeues the caller after its body returns.",
    },
    ApiEntry {
        library: "exec.library",
        name: "GetTaskID",
        syscall: Some(15),
        status: Status::Verified,
        verified_by: Some("exec_get_task_id_returns_the_caller"),
        notes: "Returns the calling task's identifier.",
    },
    ApiEntry {
        library: "exec.library",
        name: "PutChar",
        syscall: Some(16),
        status: Status::Implemented,
        verified_by: None,
        notes: "Writes one byte to the console.",
    },
    ApiEntry {
        library: "exec.library",
        name: "PowerOff",
        syscall: Some(17),
        status: Status::Implemented,
        verified_by: None,
        notes: "Stops the machine through QEMU's debug-exit device.",
    },
    ApiEntry {
        library: "exec.library",
        name: "MapPage",
        syscall: Some(18),
        status: Status::Verified,
        verified_by: Some("exec_map_page_maps_a_page_for_the_caller"),
        notes: "Maps one page into the calling task's own address space.",
    },
    ApiEntry {
        library: "exec.library",
        name: "Brk",
        syscall: Some(19),
        status: Status::Verified,
        verified_by: Some("exec_brk_grows_and_shrinks_the_break"),
        notes:
            "Returns the break actually in force; growing maps pages and shrinking releases them.",
    },
    ApiEntry {
        library: "exec.library",
        name: "AddPort",
        syscall: Some(20),
        status: Status::Verified,
        verified_by: Some("exec_add_port_reports_the_ports_signal_mask"),
        notes: "Returns the port's whole `mp_sig` mask; another task's port is `NOT_OWNER`.",
    },
    ApiEntry {
        library: "exec.library",
        name: "FindTask",
        syscall: Some(43),
        status: Status::Verified,
        verified_by: Some("exec_find_task_finds_a_named_task"),
        notes: "A null name asks for the calling task; an unknown name answers `0`, \
                never an error.",
    },
    ApiEntry {
        library: "exec.library",
        name: "SetTaskName",
        syscall: Some(44),
        status: Status::Verified,
        verified_by: Some("exec_find_task_finds_a_named_task"),
        notes: "Names the calling task, the way a task sets `tc_Node.ln_Name`.",
    },
    ApiEntry {
        library: "exec.library",
        name: "InitSemaphore",
        syscall: Some(45),
        status: Status::Verified,
        verified_by: Some("exec_semaphore_lifecycle_is_verified"),
        notes: "Creates a kernel-owned semaphore and returns its id; the classic \
                `priority` argument orders the wait queue.",
    },
    ApiEntry {
        library: "exec.library",
        name: "ObtainSemaphore",
        syscall: Some(46),
        status: Status::Verified,
        verified_by: Some("exec_semaphore_grant_reaches_the_waiter"),
        notes: "`1` when the lock is taken, `0` when the caller was queued; the \
                grant arrives as `SIGF_SINGLE`.",
    },
    ApiEntry {
        library: "exec.library",
        name: "ConfirmGrant",
        syscall: Some(47),
        status: Status::Verified,
        verified_by: Some("exec_semaphore_grant_reaches_the_waiter"),
        notes: "Consumes the pending grant wait and reports whether the hand-off \
                named the caller; a grant cannot be confirmed twice.",
    },
    ApiEntry {
        library: "exec.library",
        name: "AttemptSemaphore",
        syscall: Some(48),
        status: Status::Verified,
        verified_by: Some("exec_attempt_semaphore_reports_failure_without_blocking"),
        notes: "Never blocks: `0` says the lock was held, which is an answer \
                rather than an error.",
    },
    ApiEntry {
        library: "exec.library",
        name: "ReleaseSemaphore",
        syscall: Some(49),
        status: Status::Verified,
        verified_by: Some("exec_semaphore_lifecycle_is_verified"),
        notes: "Releases one nesting level and grants the queue onward; a release \
                the caller does not own is `NOT_OWNER`.",
    },
    ApiEntry {
        library: "exec.library",
        name: "DeleteSemaphore",
        syscall: Some(50),
        status: Status::Verified,
        verified_by: Some("exec_semaphore_lifecycle_is_verified"),
        notes: "Wakes the waiters so their next attempt reports `NO_OBJECT` \
                rather than sleeping forever.",
    },
    ApiEntry {
        library: "exec.library",
        name: "Alert",
        syscall: Some(51),
        status: Status::Verified,
        verified_by: Some("exec_alert_is_accepted_and_a_dead_alert_ends_the_task"),
        notes: "Logs the alert with its decoded name; the `AT_DeadEnd` type bit \
                ends the calling task.",
    },
    // --- dos.library -----------------------------------------------------
    ApiEntry {
        library: "dos.library",
        name: "Open",
        syscall: Some(256),
        status: Status::Verified,
        verified_by: Some("dos_open_write_read_close"),
        notes: "Opening for writing creates the file.",
    },
    ApiEntry {
        library: "dos.library",
        name: "Close",
        syscall: Some(257),
        status: Status::Verified,
        verified_by: Some("dos_open_write_read_close"),
        notes: "Closing a handle that is not open reports `NO_OBJECT`.",
    },
    ApiEntry {
        library: "dos.library",
        name: "Read",
        syscall: Some(258),
        status: Status::Verified,
        verified_by: Some("dos_open_write_read_close"),
        notes: "The data is read first and copied out second.",
    },
    ApiEntry {
        library: "dos.library",
        name: "Write",
        syscall: Some(259),
        status: Status::Verified,
        verified_by: Some("dos_open_write_read_close"),
        notes: "The copy in is first: the write cannot be undone.",
    },
    ApiEntry {
        library: "dos.library",
        name: "Seek",
        syscall: Some(260),
        status: Status::Verified,
        verified_by: Some("dos_seek_moves_the_position"),
        notes: "`SEEK_SET`, `SEEK_CUR` and `SEEK_END` are the AmigaOS modes.",
    },
    ApiEntry {
        library: "dos.library",
        name: "ReadDir",
        syscall: Some(261),
        status: Status::Implemented,
        verified_by: None,
        notes: "Returns a fixed-size record per entry, at most 64 at a time.",
    },
    ApiEntry {
        library: "dos.library",
        name: "DeleteFile",
        syscall: Some(262),
        status: Status::Implemented,
        verified_by: None,
        notes: "Refuses to remove a directory.",
    },
    ApiEntry {
        library: "dos.library",
        name: "MakeDir",
        syscall: Some(263),
        status: Status::Implemented,
        verified_by: None,
        notes: "Creates a directory in an existing parent.",
    },
    ApiEntry {
        library: "dos.library",
        name: "Examine",
        syscall: Some(264),
        status: Status::Implemented,
        verified_by: None,
        notes: "Fills in a `FileInfo` in user space.",
    },
    ApiEntry {
        library: "dos.library",
        name: "CurrentDir",
        syscall: Some(265),
        status: Status::Implemented,
        verified_by: None,
        notes: "Returns the process's current directory as a NUL-terminated string.",
    },
    ApiEntry {
        library: "dos.library",
        name: "ExamineDir",
        syscall: Some(266),
        status: Status::Verified,
        verified_by: Some("dos_examine_dir_walks_a_directory_from_an_anchor"),
        notes: "Walks a directory one entry per call, resuming from the anchor it wrote.",
    },
    ApiEntry {
        library: "dos.library",
        name: "SetCurrentDir",
        syscall: Some(267),
        status: Status::Verified,
        verified_by: Some("dos_set_current_dir_resolves_relative_paths"),
        notes: "Sets the current directory from a path; a relative path is resolved against the current one.",
    },
    // --- graphics.library ------------------------------------------------
    ApiEntry {
        library: "graphics.library",
        name: "AllocPen",
        syscall: Some(64),
        status: Status::Verified,
        verified_by: Some("graphics_pen_lifecycle"),
        notes: "Pen zero is a legitimate pen, so failure is `0xFFFF`.",
    },
    ApiEntry {
        library: "graphics.library",
        name: "FreePen",
        syscall: Some(65),
        status: Status::Verified,
        verified_by: Some("graphics_pen_lifecycle"),
        notes: "Releasing a pen twice reports `NO_OBJECT`.",
    },
    ApiEntry {
        library: "graphics.library",
        name: "SetPen",
        syscall: Some(66),
        status: Status::Verified,
        verified_by: Some("graphics_pen_lifecycle"),
        notes: "Returns the colour the pen resolves to, packed `0x00RRGGBB`.",
    },
    ApiEntry {
        library: "graphics.library",
        name: "Rect",
        syscall: Some(67),
        status: Status::Verified,
        verified_by: Some("graphics_rect_needs_a_valid_pointer"),
        notes: "The rectangle arrives behind a pointer, so it is validated.",
    },
    ApiEntry {
        library: "graphics.library",
        name: "FillRect",
        syscall: Some(68),
        status: Status::Verified,
        verified_by: Some("graphics_rect_needs_a_valid_pointer"),
        notes: "The rectangle arrives behind a pointer, so it is validated.",
    },
    ApiEntry {
        library: "graphics.library",
        name: "Text",
        syscall: Some(69),
        status: Status::Verified,
        verified_by: Some("graphics_text_draws_into_the_window"),
        notes: "Returns the advance width in pixels.",
    },
    ApiEntry {
        library: "graphics.library",
        name: "Line",
        syscall: Some(70),
        status: Status::Implemented,
        verified_by: None,
        notes: "Draws a segment into the calling task's window.",
    },
    ApiEntry {
        library: "graphics.library",
        name: "SetClip",
        syscall: Some(71),
        status: Status::Implemented,
        verified_by: None,
        notes: "Sets the window's clip rectangle.",
    },
    ApiEntry {
        library: "graphics.library",
        name: "PenColor",
        syscall: Some(72),
        status: Status::Implemented,
        verified_by: None,
        notes: "The colour a pen resolves to.",
    },
    ApiEntry {
        library: "graphics.library",
        name: "WritePixel",
        syscall: Some(73),
        status: Status::Implemented,
        verified_by: None,
        notes: "Plots one pixel in the calling task's window.",
    },
    ApiEntry {
        library: "graphics.library",
        name: "Move",
        syscall: Some(74),
        status: Status::Implemented,
        verified_by: None,
        notes: "Sets the drawing position and plots it.",
    },
    ApiEntry {
        library: "graphics.library",
        name: "SetPalette",
        syscall: Some(75),
        status: Status::Implemented,
        verified_by: None,
        notes: "Installs a whole palette.",
    },
    ApiEntry {
        library: "graphics.library",
        name: "RasterInfo",
        syscall: Some(76),
        status: Status::Implemented,
        verified_by: None,
        notes: "The raster's geometry.",
    },
    // --- layers.library --------------------------------------------------
    ApiEntry {
        library: "layers.library",
        name: "CreateLayer",
        syscall: Some(128),
        status: Status::Verified,
        verified_by: Some("layers_create_and_destroy"),
        notes: "Returns a handle for the new layer.",
    },
    ApiEntry {
        library: "layers.library",
        name: "DestroyLayer",
        syscall: Some(129),
        status: Status::Verified,
        verified_by: Some("layers_create_and_destroy"),
        notes: "Destroying a layer that does not exist reports `NO_OBJECT`.",
    },
    ApiEntry {
        library: "layers.library",
        name: "SetClip",
        syscall: Some(130),
        status: Status::Implemented,
        verified_by: None,
        notes: "Sets the layer's clip rectangle.",
    },
    ApiEntry {
        library: "layers.library",
        name: "Front",
        syscall: Some(131),
        status: Status::Implemented,
        verified_by: None,
        notes: "Brings a layer to the front of the stack.",
    },
    ApiEntry {
        library: "layers.library",
        name: "Back",
        syscall: Some(132),
        status: Status::Implemented,
        verified_by: None,
        notes: "Sends a layer to the back of the stack.",
    },
    ApiEntry {
        library: "layers.library",
        name: "SetVisible",
        syscall: Some(133),
        status: Status::Implemented,
        verified_by: None,
        notes: "Shows or hides a layer.",
    },
    ApiEntry {
        library: "layers.library",
        name: "MoveLayer",
        syscall: Some(134),
        status: Status::Implemented,
        verified_by: None,
        notes: "Moves or resizes a layer.",
    },
    ApiEntry {
        library: "layers.library",
        name: "Composite",
        syscall: Some(135),
        status: Status::Implemented,
        verified_by: None,
        notes: "Composites the stack into the framebuffer.",
    },
    ApiEntry {
        library: "layers.library",
        name: "Order",
        syscall: Some(136),
        status: Status::Implemented,
        verified_by: None,
        notes: "The z-order, back to front.",
    },
    ApiEntry {
        library: "layers.library",
        name: "Topmost",
        syscall: Some(137),
        status: Status::Implemented,
        verified_by: None,
        notes: "The topmost layer at a point.",
    },
    // --- intuition.library ------------------------------------------------
    ApiEntry {
        library: "intuition.library",
        name: "OpenScreen",
        syscall: Some(192),
        status: Status::Verified,
        verified_by: Some("intuition_screen_and_window_lifecycle"),
        notes: "A zero-sized screen is refused with `BAD_RECT`.",
    },
    ApiEntry {
        library: "intuition.library",
        name: "CloseScreen",
        syscall: Some(193),
        status: Status::Verified,
        verified_by: Some("intuition_screen_and_window_lifecycle"),
        notes: "Closing a screen closes the windows on it and reports how many.",
    },
    ApiEntry {
        library: "intuition.library",
        name: "OpenWindow",
        syscall: Some(194),
        status: Status::Verified,
        verified_by: Some("intuition_screen_and_window_lifecycle"),
        notes: "The geometry arrives behind a pointer and is validated.",
    },
    ApiEntry {
        library: "intuition.library",
        name: "CloseWindow",
        syscall: Some(195),
        status: Status::Verified,
        verified_by: Some("intuition_screen_and_window_lifecycle"),
        notes: "Closing another task's window reports `NOT_OWNER`.",
    },
    ApiEntry {
        library: "intuition.library",
        name: "SetTitle",
        syscall: Some(196),
        status: Status::Verified,
        verified_by: Some("intuition_window_title_and_gadgets"),
        notes: "The title is copied out of user space before it is stored.",
    },
    ApiEntry {
        library: "intuition.library",
        name: "Refresh",
        syscall: Some(197),
        status: Status::Implemented,
        verified_by: None,
        notes: "Repaints every window; a non-zero flag also composites.",
    },
    ApiEntry {
        library: "intuition.library",
        name: "SetAttrs",
        syscall: Some(198),
        status: Status::Verified,
        verified_by: Some("intuition_window_title_and_gadgets"),
        notes: "Sets the window's flags and IDCMP mask.",
    },
    ApiEntry {
        library: "intuition.library",
        name: "AddGadget",
        syscall: Some(199),
        status: Status::Verified,
        verified_by: Some("intuition_window_title_and_gadgets"),
        notes: "The gadget's rectangle arrives behind a pointer and is validated.",
    },
    ApiEntry {
        library: "intuition.library",
        name: "RemoveGadget",
        syscall: Some(200),
        status: Status::Implemented,
        verified_by: None,
        notes: "Detaches a gadget from its window.",
    },
    ApiEntry {
        library: "intuition.library",
        name: "SetBounds",
        syscall: Some(201),
        status: Status::Implemented,
        verified_by: None,
        notes: "Moves or resizes a window; zero keeps the current dimension.",
    },
    ApiEntry {
        library: "intuition.library",
        name: "Front",
        syscall: Some(202),
        status: Status::Implemented,
        verified_by: None,
        notes: "Brings a window to the front of the layer stack.",
    },
    ApiEntry {
        library: "intuition.library",
        name: "Back",
        syscall: Some(203),
        status: Status::Implemented,
        verified_by: None,
        notes: "Sends a window to the back of the layer stack.",
    },
    ApiEntry {
        library: "intuition.library",
        name: "Repaint",
        syscall: Some(204),
        status: Status::Implemented,
        verified_by: None,
        notes: "Repaints a window's furniture.",
    },
    // --- utility.library -------------------------------------------------
    ApiEntry {
        library: "utility.library",
        name: "FindTag",
        syscall: Some(320),
        status: Status::Verified,
        verified_by: Some("utility_find_tag_returns_the_matching_items_data"),
        notes: "Returns the matching item's `ti_Data`, never a pointer; \
                the first match wins and `TAG_END` ends the list.",
    },
    ApiEntry {
        library: "utility.library",
        name: "StrNCpy",
        syscall: Some(321),
        status: Status::Verified,
        verified_by: Some("utility_strncpy_copies_at_most_n_bytes"),
        notes: "Writes at most `n` bytes — text then a NUL — and returns \
                how many it wrote; the SDK refuses an `n` its buffer cannot hold.",
    },
    ApiEntry {
        library: "utility.library",
        name: "StrCmp",
        syscall: Some(322),
        status: Status::Verified,
        verified_by: Some("utility_strcmp_orders_two_strings"),
        notes: "Byte-wise and unsigned; `less` folds to `u64::MAX`, which the \
                jump table truncates to `0xFFFF_FFFF` and the SDK still decodes as `Less`.",
    },
    // --- icon.library ----------------------------------------------------
    ApiEntry {
        library: "icon.library",
        name: "GetDiskObject",
        syscall: Some(384),
        status: Status::Verified,
        verified_by: Some("icon_disk_object_lifecycle_names_and_duplicates"),
        notes: "Finds an icon by its exact name; a missing name is `NOT_FOUND`, \
                never a null handle.",
    },
    ApiEntry {
        library: "icon.library",
        name: "GetDiskObjectNew",
        syscall: Some(385),
        status: Status::Verified,
        verified_by: Some("icon_disk_object_lifecycle_names_and_duplicates"),
        notes: "Finds an icon, or creates a default one under that name; the \
                second call returns the same handle as the first.",
    },
    ApiEntry {
        library: "icon.library",
        name: "PutDiskObject",
        syscall: Some(386),
        status: Status::Verified,
        verified_by: Some("icon_disk_object_lifecycle_names_and_duplicates"),
        notes: "Names an existing icon handle; the name is copied, not retained.",
    },
    ApiEntry {
        library: "icon.library",
        name: "FreeDiskObject",
        syscall: Some(387),
        status: Status::Verified,
        verified_by: Some("icon_disk_object_lifecycle_names_and_duplicates"),
        notes: "Releases a handle; the slot stays reserved so other handles do \
                not shift, and a second free is `NO_OBJECT`.",
    },
    ApiEntry {
        library: "icon.library",
        name: "DupDiskObject",
        syscall: Some(388),
        status: Status::Verified,
        verified_by: Some("icon_disk_object_lifecycle_names_and_duplicates"),
        notes: "Copies an icon into a fresh handle that can be freed independently.",
    },
    ApiEntry {
        library: "icon.library",
        name: "DeleteDiskObject",
        syscall: Some(389),
        status: Status::Verified,
        verified_by: Some("icon_disk_object_lifecycle_names_and_duplicates"),
        notes: "Removes the icon with the given name, then a lookup reports \
                `NOT_FOUND`.",
    },
    ApiEntry {
        library: "icon.library",
        name: "GetDefDiskObject",
        syscall: Some(390),
        status: Status::Verified,
        verified_by: Some("icon_defaults_and_tool_types"),
        notes: "Returns a copy of the default object for a kind, so editing it \
                cannot mutate the default.",
    },
    ApiEntry {
        library: "icon.library",
        name: "PutDefDiskObject",
        syscall: Some(391),
        status: Status::Verified,
        verified_by: Some("icon_defaults_and_tool_types"),
        notes: "Replaces the default object for a kind; a later `GetDefDiskObject` \
                observes the replacement.",
    },
    ApiEntry {
        library: "icon.library",
        name: "NewDiskObject",
        syscall: Some(392),
        status: Status::Verified,
        verified_by: Some("icon_defaults_and_tool_types"),
        notes: "A fresh default object of a kind, carrying `TYPE=<kind>` as a \
                ToolType; an unknown kind is `INVALID_NAME`.",
    },
    ApiEntry {
        library: "icon.library",
        name: "FindToolType",
        syscall: Some(393),
        status: Status::Verified,
        verified_by: Some("icon_defaults_and_tool_types"),
        notes: "Matches a ToolType name case-insensitively and copies the value \
                plus its NUL terminator into the caller's buffer.",
    },
    ApiEntry {
        library: "icon.library",
        name: "MatchToolValue",
        syscall: Some(394),
        status: Status::Verified,
        verified_by: Some("icon_defaults_and_tool_types"),
        notes: "Compares a ToolType value case-insensitively; a missing ToolType \
                is `false`, not an error.",
    },
    ApiEntry {
        library: "icon.library",
        name: "GetIconTagList",
        syscall: Some(395),
        status: Status::Verified,
        verified_by: Some("icon_tag_list_round_trip"),
        notes: "Copies up to `max_count` tags into the caller's buffer and \
                returns how many it copied.",
    },
    ApiEntry {
        library: "icon.library",
        name: "PutIconTagList",
        syscall: Some(396),
        status: Status::Verified,
        verified_by: Some("icon_tag_list_round_trip"),
        notes: "Replaces an icon's tag list; more than 64 tags is `TOO_LARGE`, \
                never a silent truncation.",
    },
    ApiEntry {
        library: "icon.library",
        name: "BumpRevision",
        syscall: Some(397),
        status: Status::Verified,
        verified_by: Some("icon_geometry_revision_and_state"),
        notes: "Increments and returns the icon's revision counter.",
    },
    ApiEntry {
        library: "icon.library",
        name: "LayoutIcon",
        syscall: Some(398),
        status: Status::Verified,
        verified_by: Some("icon_geometry_revision_and_state"),
        notes: "Stores a rectangle read from a caller buffer and writes the one \
                in force back; a zero width or height is `BAD_RECT`.",
    },
    ApiEntry {
        library: "icon.library",
        name: "GetIconRectangle",
        syscall: Some(399),
        status: Status::Verified,
        verified_by: Some("icon_geometry_revision_and_state"),
        notes: "Writes the icon's rectangle into a caller-owned buffer; never a \
                kernel pointer.",
    },
    ApiEntry {
        library: "icon.library",
        name: "IconControl",
        syscall: Some(400),
        status: Status::Verified,
        verified_by: Some("icon_geometry_revision_and_state"),
        notes: "Selects, deselects, queries the selected state, and gets or sets \
                the icon kind; an unknown operation is `RANGE`.",
    },
    ApiEntry {
        library: "icon.library",
        name: "DrawIconState",
        syscall: Some(401),
        status: Status::Verified,
        verified_by: Some("icon_geometry_revision_and_state"),
        notes: "Records whether the icon is drawn selected; the desktop reads the \
                state through `IconControl`.",
    },
    ApiEntry {
        library: "icon.library",
        name: "AddFreeList",
        syscall: Some(402),
        status: Status::Verified,
        verified_by: Some("icon_free_list_and_selected_colour"),
        notes: "Remembers a live handle, ignoring a duplicate, and returns the \
                list's length.",
    },
    ApiEntry {
        library: "icon.library",
        name: "FreeFreeList",
        syscall: Some(403),
        status: Status::Verified,
        verified_by: Some("icon_free_list_and_selected_colour"),
        notes: "Clears the remembered handles and returns how many it dropped.",
    },
    ApiEntry {
        library: "icon.library",
        name: "ChangeToSelectedIconColor",
        syscall: Some(404),
        status: Status::Verified,
        verified_by: Some("icon_free_list_and_selected_colour"),
        notes: "Sets the selected-icon colour word and returns the previous one.",
    },
    // --- workbench.library -----------------------------------------------
    ApiEntry {
        library: "workbench.library",
        name: "AddAppIcon",
        syscall: Some(448),
        status: Status::Verified,
        verified_by: Some("workbench_app_icons_walk_and_remove"),
        notes: "Registers an application icon; an empty name or path, or an \
                unknown kind, is `INVALID_NAME`.",
    },
    ApiEntry {
        library: "workbench.library",
        name: "AddAppMenuItem",
        syscall: Some(449),
        status: Status::Verified,
        verified_by: Some("workbench_menu_items_lifecycle"),
        notes: "Registers a menu item with its action code; an empty label is \
                `INVALID_NAME`.",
    },
    ApiEntry {
        library: "workbench.library",
        name: "AddAppWindow",
        syscall: Some(450),
        status: Status::Verified,
        verified_by: Some("workbench_app_windows_drop_zones_and_messages"),
        notes: "Registers a window owned by the calling task; an unknown window \
                is `NO_OBJECT`.",
    },
    ApiEntry {
        library: "workbench.library",
        name: "AddAppWindowDropZone",
        syscall: Some(451),
        status: Status::Verified,
        verified_by: Some("workbench_app_windows_drop_zones_and_messages"),
        notes: "Registers a drop zone on a live window; a zero-area rectangle is \
                `BAD_RECT`.",
    },
    ApiEntry {
        library: "workbench.library",
        name: "CloseWorkbenchObject",
        syscall: Some(452),
        status: Status::Verified,
        verified_by: Some("workbench_objects_open_update_close"),
        notes: "Marks an object closed and invisible; the record stays so its \
                handle remains a stable name.",
    },
    ApiEntry {
        library: "workbench.library",
        name: "GetNextAppIcon",
        syscall: Some(453),
        status: Status::Verified,
        verified_by: Some("workbench_app_icons_walk_and_remove"),
        notes: "Walks the icon table after a cursor; zero is a normal end of \
                iteration, not an error.",
    },
    ApiEntry {
        library: "workbench.library",
        name: "MakeWorkbenchObjectVisible",
        syscall: Some(454),
        status: Status::Verified,
        verified_by: Some("workbench_objects_open_update_close"),
        notes: "Sets an object's visibility; an unknown handle is `NO_OBJECT`.",
    },
    ApiEntry {
        library: "workbench.library",
        name: "NotifyWorkbench",
        syscall: Some(455),
        status: Status::Verified,
        verified_by: Some("workbench_register_start_and_notify"),
        notes: "Puts an action/data message on the registered Exec port; no \
                registered port is `NO_PORT`.",
    },
    ApiEntry {
        library: "workbench.library",
        name: "OpenWorkbenchObject",
        syscall: Some(456),
        status: Status::Verified,
        verified_by: Some("workbench_objects_open_update_close"),
        notes: "Creates an open object record; an empty name or path is \
                `INVALID_NAME`.",
    },
    ApiEntry {
        library: "workbench.library",
        name: "QuoteWorkbench",
        syscall: Some(457),
        status: Status::Verified,
        verified_by: Some("workbench_register_start_and_notify"),
        notes: "Toggles path quoting and returns the previous state.",
    },
    ApiEntry {
        library: "workbench.library",
        name: "RegisterWorkbench",
        syscall: Some(458),
        status: Status::Verified,
        verified_by: Some("workbench_register_start_and_notify"),
        notes: "Binds the calling task's port; a second registration is \
                `OBJECT_IN_USE`, and another task's port is `NOT_OWNER`.",
    },
    ApiEntry {
        library: "workbench.library",
        name: "RemoveAppIcon",
        syscall: Some(459),
        status: Status::Verified,
        verified_by: Some("workbench_app_icons_walk_and_remove"),
        notes: "Removes an application icon, and a second removal is `NO_OBJECT`.",
    },
    ApiEntry {
        library: "workbench.library",
        name: "RemoveAppMenuItem",
        syscall: Some(460),
        status: Status::Verified,
        verified_by: Some("workbench_menu_items_lifecycle"),
        notes: "Removes a menu item, and a second removal is `NO_OBJECT`.",
    },
    ApiEntry {
        library: "workbench.library",
        name: "RemoveAppWindow",
        syscall: Some(461),
        status: Status::Verified,
        verified_by: Some("workbench_app_windows_drop_zones_and_messages"),
        notes: "Removes a window record the caller owns; another task's record is \
                `NOT_OWNER`.",
    },
    ApiEntry {
        library: "workbench.library",
        name: "RemoveAppWindowDropZone",
        syscall: Some(462),
        status: Status::Verified,
        verified_by: Some("workbench_app_windows_drop_zones_and_messages"),
        notes: "Removes a drop zone, and a second removal is `NO_OBJECT`.",
    },
    ApiEntry {
        library: "workbench.library",
        name: "SendAppWindowMessage",
        syscall: Some(463),
        status: Status::Verified,
        verified_by: Some("workbench_app_windows_drop_zones_and_messages"),
        notes: "Puts an action/data message on the window's own port; another \
                task's record is `NOT_OWNER`.",
    },
    ApiEntry {
        library: "workbench.library",
        name: "StartWorkbench",
        syscall: Some(464),
        status: Status::Verified,
        verified_by: Some("workbench_register_start_and_notify"),
        notes: "Marks the desktop running; an unregistered Workbench is \
                `NO_OBJECT`.",
    },
    ApiEntry {
        library: "workbench.library",
        name: "UnregisterWorkbench",
        syscall: Some(465),
        status: Status::Verified,
        verified_by: Some("workbench_register_start_and_notify"),
        notes: "Drops the registration and clears the running flag; a second \
                call is `NO_OBJECT`.",
    },
    ApiEntry {
        library: "workbench.library",
        name: "UpdateWorkbenchObject",
        syscall: Some(466),
        status: Status::Verified,
        verified_by: Some("workbench_objects_open_update_close"),
        notes: "Replaces an object's path and kind; an empty path is \
                `INVALID_NAME`.",
    },
    ApiEntry {
        library: "workbench.library",
        name: "WBConfig",
        syscall: Some(467),
        status: Status::Verified,
        verified_by: Some("workbench_register_start_and_notify"),
        notes: "Copies the configuration line, terminator included, into the \
                caller's buffer; a short buffer is `RANGE`.",
    },
    ApiEntry {
        library: "workbench.library",
        name: "WBInfo",
        syscall: Some(468),
        status: Status::Verified,
        verified_by: Some("workbench_register_start_and_notify"),
        notes: "Copies the information line, terminator included, into the \
                caller's buffer; a short buffer is `RANGE`.",
    },
    ApiEntry {
        library: "workbench.library",
        name: "WorkbenchControl",
        syscall: Some(469),
        status: Status::Verified,
        verified_by: Some("workbench_register_start_and_notify"),
        notes: "Reports running state, registration, and the live object counts; \
                an unknown operation is `NO_OBJECT`.",
    },
];

/// The entries of one library, in registry order.
pub fn entries_for(library: &str) -> Vec<&'static ApiEntry> {
    MATRIX.iter().filter(|e| e.library == library).collect()
}

/// The entry with the given library and name.
pub fn entry(library: &str, name: &str) -> Option<&'static ApiEntry> {
    MATRIX
        .iter()
        .find(|e| e.library == library && e.name == name)
}

/// How many entries carry each status.
pub fn status_counts() -> BTreeMap<&'static str, usize> {
    let mut counts = BTreeMap::new();
    for status in [
        Status::NotStarted,
        Status::Stub,
        Status::Implemented,
        Status::Verified,
    ] {
        counts.insert(status.as_str(), 0);
    }
    for entry in MATRIX {
        *counts.entry(entry.status.as_str()).or_insert(0) += 1;
    }
    counts
}

/// The total number of registered entries.
pub fn total_entries() -> usize {
    MATRIX.len()
}

/// The names of every behaviour test the matrix relies on.
///
/// The `tests/consistency.rs` suite checks that this list and the set of
/// `verified_by` names in the matrix are the same, so a `verified` entry can
/// never point at a test that does not exist.
pub fn verified_test_names() -> Vec<&'static str> {
    let mut names: Vec<&'static str> = MATRIX.iter().filter_map(|e| e.verified_by).collect();
    names.sort_unstable();
    names.dedup();
    names
}

/// Render the Markdown conformance matrix for one library.
///
/// The document is generated from [`MATRIX`], so it cannot drift away from
/// the code unless the table itself is wrong.
pub fn render_library_matrix(library: &str) -> String {
    let mut out = String::new();
    out.push_str(&format!("# `{library}` — API conformance matrix\n\n"));
    out.push_str(
        "> Generated from `tests/api-conformance`'s registry. Do not edit by hand:\n\
         > run `VELA_UPDATE_CONFORMANCE_DOCS=1 cargo test -p vela-test-api-conformance`\n\
         > to regenerate this file.\n\n",
    );
    out.push_str(
        "The status column uses the taxonomy from the design document (§7.6):\n\n\
         | status | meaning |\n|---|---|\n\
         | `not-started` | the API is not reachable — no syscall slot, no service method |\n\
         | `stub` | the API exists but returns a fixed placeholder |\n\
         | `implemented` | the API exists and returns a plausible value, but no test asserts the behaviour |\n\
         | `verified` | a test in `tests/api-conformance` asserts the documented behaviour |\n\n\
         **A stub is not compatible.** A stub that returned success would be\n\
         indistinguishable from a working call.\n\n",
    );
    out.push_str("| API | syscall | status | verified by | notes |\n");
    out.push_str("|---|---|---|---|---|\n");
    for entry in entries_for(library) {
        let syscall = match entry.syscall {
            Some(n) => n.to_string(),
            None => "—".to_string(),
        };
        let verified_by = match entry.verified_by {
            Some(name) => format!("`{name}`"),
            None => "—".to_string(),
        };
        out.push_str(&format!(
            "| `{}` | {} | `{}` | {} | {} |\n",
            entry.name,
            syscall,
            entry.status.as_str(),
            verified_by,
            entry.notes
        ));
    }
    out.push('\n');
    let entries = entries_for(library);
    let verified = entries.iter().filter(|e| e.status.is_verified()).count();
    out.push_str(&format!(
        "**Summary:** {} of {} entries are `verified`.\n",
        verified,
        entries.len()
    ));
    out
}

/// Render every library's matrix, as separate documents.
///
/// The key is the file name under `docs/amiga-api/`, without the extension.
pub fn render_all() -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for library in LIBRARIES {
        let stem = library.strip_suffix(".library").unwrap_or(library);
        out.insert(
            format!("{stem}-conformance-matrix.md"),
            render_library_matrix(library),
        );
    }
    out
}

/// The workspace root, found by walking up from this crate's manifest.
///
/// The crate lives at `<root>/tests/api-conformance`, so the root is two
/// levels up. `CARGO_MANIFEST_DIR` is used rather than the current directory
/// so that the test works no matter where it is invoked from.
pub fn workspace_root() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(std::path::Path::parent)
        .expect("tests/api-conformance lives two levels below the workspace root")
        .to_path_buf()
}

/// The shared harness for the behaviour tests.
///
/// The helpers live in the library rather than in a `tests/support` module
/// so that every test binary can use them without the *unused* warnings a
/// shared module would produce in the binaries that do not need every
/// helper. The project bans lint suppressions, and the alternative to
/// putting them here would have been to write each one twice.
///
/// # The buffers are leaked on purpose
///
/// The kernel copies data *out of* the user buffers it is given, so a buffer
/// must still be alive when the syscall runs. A `Vec` on the test's stack
/// would move when the stack frame moves, and a pointer into it would then
/// name a different address. Leaking the buffer removes the question
/// entirely: the address never changes and never becomes invalid.
pub mod harness {
    use std::sync::Arc;

    use vela_kernel_exec::task::TaskId;
    use vela_kernel_syscall::numbers::SyscallArgs;
    use vela_kernel_syscall::SyscallResult;
    use vela_ramfs::RamFs;

    use vela_hosted::Kernel;

    /// A mock kernel with a ramfs mounted at `/ramfs`, which is where `RAM:`
    /// points.
    pub fn kernel() -> Kernel {
        let k = Kernel::new();
        let ram = Arc::new(RamFs::new());
        k.dos().mount("/ramfs", "RAM", ram);
        k
    }

    /// A fresh task id, created through the Exec service.
    pub fn task(k: &Kernel) -> TaskId {
        k.exec().create_task()
    }

    /// Dispatch a syscall as `task`.
    pub fn call(k: &Kernel, task: TaskId, number: u64, args: SyscallArgs) -> SyscallResult {
        k.syscall(task, number, args)
    }

    /// The result of a syscall that must have succeeded.
    pub fn ok(result: SyscallResult, what: &str) -> SyscallResult {
        assert!(
            result.is_ok(),
            "{what} failed with error {} (value {})",
            result.error,
            result.value
        );
        result
    }

    /// A NUL-terminated string in a buffer that outlives the test.
    pub fn cstring(text: &str) -> u64 {
        let mut bytes = text.as_bytes().to_vec();
        bytes.push(0);
        let boxed = bytes.into_boxed_slice();
        Box::leak(boxed).as_ptr() as u64
    }

    /// A byte buffer that outlives the test.
    pub fn bytes(data: &[u8]) -> u64 {
        let boxed = data.to_vec().into_boxed_slice();
        Box::leak(boxed).as_ptr() as u64
    }

    /// The address of a `#[repr(C)]` structure, in a buffer that outlives the
    /// test.
    ///
    /// The structure's bytes are copied into the buffer, so the address the
    /// kernel is given names the structure's layout rather than whatever
    /// happens to be on the test's stack.
    pub fn struct_bytes<T: Copy>(value: &T) -> u64 {
        let size = std::mem::size_of::<T>();
        let mut buffer = vec![0u8; size];
        // SAFETY: `value` is a live borrow, so the pointer is valid for the
        // `size_of::<T>()` bytes that are read. The length is the type's own
        // size and `buffer` is exactly that long, so neither side can go out
        // of bounds.
        buffer.copy_from_slice(unsafe {
            std::slice::from_raw_parts(value as *const T as *const u8, size)
        });
        bytes(&buffer)
    }

    /// An address in the kernel half, which the kernel must refuse to touch.
    ///
    /// On bare metal this names the kernel's own image. The kernel must
    /// reject it *before* dereferencing it — a kernel that read from an
    /// address it was handed would be handing the user program a privilege
    /// escalation.
    pub const KERNEL_ADDRESS: u64 = 0xFFFF_8000_0000_0000;

    /// An address in the user half that is not mapped, so the probe must fail.
    ///
    /// The address is canonical and low, so the range check passes and the
    /// *probe* is what refuses it — which is the case a range check alone
    /// cannot catch.
    pub const UNMAPPED_USER_ADDRESS: u64 = 0x0000_1000_0000_0000;
}

/// A byte buffer the kernel may write into, kept alive for the whole test.
///
/// The buffer is *not* copied when it is handed to the kernel: the pointer
/// the kernel gets is the pointer the test reads back through, so a copy-out
/// is observable. Copying would have made every assertion about the copied
/// data vacuous.
pub struct ByteBuffer {
    /// The address the kernel is given.
    addr: u64,
    /// The same memory, as a raw pointer.
    raw: *mut u8,
    /// The buffer's length.
    len: usize,
}

impl ByteBuffer {
    /// A zeroed buffer of `len` bytes.
    pub fn zeroed(len: usize) -> Self {
        let raw = Box::into_raw(vec![0u8; len].into_boxed_slice()) as *mut u8;
        Self {
            addr: raw as u64,
            raw,
            len,
        }
    }

    /// A buffer holding `data`.
    pub fn with(data: &[u8]) -> Self {
        let raw = Box::into_raw(data.to_vec().into_boxed_slice()) as *mut u8;
        Self {
            addr: raw as u64,
            raw,
            len: data.len(),
        }
    }

    /// The address to hand to the kernel.
    pub fn addr(&self) -> u64 {
        self.addr
    }

    /// The bytes as the kernel left them.
    pub fn read(&self) -> Vec<u8> {
        // SAFETY: the allocation was leaked, so it outlives the `ByteBuffer`,
        // and nothing else moves or frees it.
        unsafe { std::slice::from_raw_parts(self.raw, self.len) }.to_vec()
    }
}
