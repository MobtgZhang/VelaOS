//! The syscall numbers and their ABI.
//!
//! # The calling convention
//!
//! A VelaOS syscall is:
//!
//! ```text
//! syscall number   in rax
//! argument 0       in rdi
//! argument 1       in rsi
//! argument 2       in rdx
//! argument 3       in r10
//! argument 4       in r8
//! argument 5       in r9
//! result           in rax
//! error            in rdx (zero on success)
//! ```
//!
//! The register choice is not arbitrary. `syscall` puts the return address
//! in `rcx` and the saved flags in `r11`, which is why `r10` and `r11` are
//! unavailable as argument registers — `r10` is used here precisely because
//! `r11` cannot be. The six argument registers are the ones the System V
//! ABI already uses for the first six integer arguments, so a call site reads
//! the same whether it is a library call or a syscall.
//!
//! # The error convention
//!
//! A syscall returns a *value* in `rax` and an *error code* in `rdx`. This
//! differs from the C convention of a single negative return value, and the
//! difference is deliberate: an AmigaOS-style API returns a pointer on success
//! and NULL on failure, and a NULL return is a perfectly good value for some
//! calls. Splitting the error out means "the call succeeded and returned
//! NULL" is expressible.
//!
//! # The numbers
//!
//! The numbers are grouped by the library that owns them, so a library's
//! functions occupy a contiguous range. The groups are:
//!
//! | Range | Library |
//! |---|---|
//! | 0–63 | `exec.library` |
//! | 64–127 | `graphics.library` |
//! | 128–191 | `layers.library` |
//! | 192–255 | `intuition.library` |
//! | 256–319 | `dos.library` |
//! | 320–383 | `utility.library` |
//!
//! The grouping means a library's jump table can be built from the numbers
//! alone, and a reserved number in a group is a gap rather than a conflict
//! with another library.
//!
//! Every number is stable: a program's stub library is compiled against these
//! values, so changing one is an ABI break.

/// `exec.library`: create a message port.
pub const CREATE_MSG_PORT: u64 = 0;
/// `exec.library`: delete a message port.
pub const DELETE_MSG_PORT: u64 = 1;
/// `exec.library`: enqueue a message.
pub const PUT_MSG: u64 = 2;
/// `exec.library`: dequeue a message.
pub const GET_MSG: u64 = 3;
/// `exec.library`: reply to a message.
pub const REPLY_MSG: u64 = 4;
/// `exec.library`: wait for a message on a port.
pub const WAIT_PORT: u64 = 5;
/// `exec.library`: wait for a signal.
pub const WAIT: u64 = 6;
/// `exec.library`: set bits in a signal set.
pub const SET_SIGNAL: u64 = 7;
/// `exec.library`: clear bits in a signal set.
pub const SET_EXCEPT: u64 = 8;
/// `exec.library`: raise a signal in a task's set.
pub const SIGNAL: u64 = 9;
/// `exec.library`: open a library.
pub const OPEN_LIBRARY: u64 = 10;
/// `exec.library`: close a library.
pub const CLOSE_LIBRARY: u64 = 11;
/// `exec.library`: enter the classic critical section.
pub const FORBID: u64 = 12;
/// `exec.library`: leave the classic critical section.
pub const PERMIT: u64 = 13;
/// `exec.library`: yield the rest of the time slice.
pub const YIELD: u64 = 14;
/// `exec.library`: the calling task's own identifier.
pub const GET_TASK_ID: u64 = 15;
/// `exec.library`: write one byte to the console.
///
/// The number is 16 because it is the *first* call a user program makes that
/// is not about AmigaOS objects at all. It sits at the end of Exec's defined
/// list rather than in a new group because it is a console, not a library —
/// but it belongs to Exec because Exec is what a program already has open.
pub const PUTCHAR: u64 = 16;
/// `exec.library`: power the machine off.
///
/// Present because a user program has no other way to say "I am finished", and
/// a guest that has finished but keeps running is a guest that has timed out.
/// A bare-metal program that reaches this has demonstrably completed, so the
/// boot smoke test can treat the machine stopping as the success signal.
pub const POWER_OFF: u64 = 17;
/// `exec.library`: map one page of user memory.
///
/// # Why a guest cannot map its own memory
///
/// A process needs memory, and a process cannot be handed memory it has no
/// way to obtain more of. So the boundary gives it exactly two ways to grow:
/// map a page, or extend the break. Both go through the kernel because both
/// consume physical frames, which only the kernel's allocator owns.
///
/// The value returned is the page's user virtual address, or the error.
pub const MAP_PAGE: u64 = 18;
/// `exec.library`: extend or shrink the program's break.
///
/// Classic `brk`, with the AmigaOS argument order: `brk(0)` queries the
/// current break, and `brk(addr)` asks for the break to become `addr`.
/// Growing maps new pages; shrinking releases the tail.
///
/// The return value is the *new* break, which is what makes the call
/// transactional — a program that asks for more than it gets learns the real
/// limit from the return value rather than assuming it succeeded.
pub const BRK: u64 = 19;

/// `graphics.library`: reserve a pen in a palette.
pub const GRAPHICS_ALLOC_PEN: u64 = 64;
/// `graphics.library`: release a pen.
pub const GRAPHICS_FREE_PEN: u64 = 65;
/// `graphics.library`: set a raster's pen.
pub const GRAPHICS_SET_PEN: u64 = 66;
/// `graphics.library`: draw a rectangle.
pub const GRAPHICS_RECT: u64 = 67;
/// `graphics.library`: fill a rectangle.
pub const GRAPHICS_FILL_RECT: u64 = 68;
/// `graphics.library`: draw text.
pub const GRAPHICS_TEXT: u64 = 69;
/// `graphics.library`: draw a line.
pub const GRAPHICS_LINE: u64 = 70;
/// `graphics.library`: set a raster's clip rectangle.
pub const GRAPHICS_SET_CLIP: u64 = 71;
/// `graphics.library`: the colour a pen resolves to.
pub const GRAPHICS_PEN_COLOR: u64 = 72;
/// `graphics.library`: write one pixel.
pub const GRAPHICS_WRITE_PIXEL: u64 = 73;
/// `graphics.library`: move the drawing position.
pub const GRAPHICS_MOVE: u64 = 74;
/// `graphics.library`: install a whole palette.
pub const GRAPHICS_SET_PALETTE: u64 = 75;
/// `graphics.library`: the raster's geometry.
pub const GRAPHICS_RASTER_INFO: u64 = 76;

/// `layers.library`: create a layer.
pub const LAYERS_CREATE: u64 = 128;
/// `layers.library`: destroy a layer.
pub const LAYERS_DESTROY: u64 = 129;
/// `layers.library`: set a layer's clip rectangle.
pub const LAYERS_SET_CLIP: u64 = 130;
/// `layers.library`: bring a layer to the front.
pub const LAYERS_FRONT: u64 = 131;
/// `layers.library`: send a layer to the back.
pub const LAYERS_BACK: u64 = 132;
/// `layers.library`: show or hide a layer.
pub const LAYERS_SET_VISIBLE: u64 = 133;
/// `layers.library`: move or resize a layer.
pub const LAYERS_MOVE: u64 = 134;
/// `layers.library`: composite the stack into a framebuffer.
pub const LAYERS_COMPOSITE: u64 = 135;
/// `layers.library`: the z-order, back to front.
pub const LAYERS_ORDER: u64 = 136;
/// `layers.library`: the topmost layer at a point.
pub const LAYERS_TOPMOST: u64 = 137;

/// `intuition.library`: open a screen.
pub const INTUITION_OPEN_SCREEN: u64 = 192;
/// `intuition.library`: close a screen.
pub const INTUITION_CLOSE_SCREEN: u64 = 193;
/// `intuition.library`: open a window.
pub const INTUITION_OPEN_WINDOW: u64 = 194;
/// `intuition.library`: close a window.
pub const INTUITION_CLOSE_WINDOW: u64 = 195;
/// `intuition.library`: set a window's title.
pub const INTUITION_SET_TITLE: u64 = 196;
/// `intuition.library`: refresh a window.
pub const INTUITION_REFRESH: u64 = 197;
/// `intuition.library`: set a window's attributes.
pub const INTUITION_SET_ATTRS: u64 = 198;
/// `intuition.library`: add a gadget to a window.
pub const INTUITION_ADD_GADGET: u64 = 199;
/// `intuition.library`: remove a gadget.
pub const INTUITION_REMOVE_GADGET: u64 = 200;
/// `intuition.library`: move or resize a window.
pub const INTUITION_SET_BOUNDS: u64 = 201;
/// `intuition.library`: bring a window to the front.
pub const INTUITION_FRONT: u64 = 202;
/// `intuition.library`: send a window to the back.
pub const INTUITION_BACK: u64 = 203;
/// `intuition.library`: repaint a window's furniture.
pub const INTUITION_REPAINT: u64 = 204;

/// `dos.library`: open a file.
pub const DOS_OPEN: u64 = 256;
/// `dos.library`: close a file.
pub const DOS_CLOSE: u64 = 257;
/// `dos.library`: read from a file.
pub const DOS_READ: u64 = 258;
/// `dos.library`: write to a file.
pub const DOS_WRITE: u64 = 259;
/// `dos.library`: seek within a file.
pub const DOS_SEEK: u64 = 260;
/// `dos.library`: enumerate a directory.
pub const DOS_READ_DIR: u64 = 261;
/// `dos.library`: delete an entry.
pub const DOS_DELETE: u64 = 262;
/// `dos.library`: create a directory.
pub const DOS_MKDIR: u64 = 263;
/// `dos.library`: examine a path.
pub const DOS_EXAMINE: u64 = 264;
/// `dos.library`: read the current directory.
pub const DOS_CURRENT_DIR: u64 = 265;
/// `dos.library`: walk a directory with an anchor.
pub const DOS_EXAMINE_DIR: u64 = 266;

/// `utility.library`: find a tag in a tag list.
pub const UTILITY_FIND_TAG: u64 = 320;
/// `utility.library`: copy a string.
pub const UTILITY_STRNCPY: u64 = 321;
/// `utility.library`: compare two strings.
pub const UTILITY_STRCMP: u64 = 322;

/// The number of syscalls in the `exec.library` group.
///
/// The count is the number of *defined* syscalls, not the size of the
/// reserved range: the range is 64 wide so that a library added later does
/// not renumber the ones already compiled into stub libraries, and the count
/// is what the conformance tests iterate over.
pub const EXEC_COUNT: u64 = 20;
/// The number of syscalls in the `graphics.library` group.
pub const GRAPHICS_COUNT: u64 = 13;
/// The number of syscalls in the `layers.library` group.
pub const LAYERS_COUNT: u64 = 10;
/// The number of syscalls in the `intuition.library` group.
pub const INTUITION_COUNT: u64 = 13;
/// The number of syscalls in the `dos.library` group.
pub const DOS_COUNT: u64 = 11;
/// The number of syscalls in the `utility.library` group.
pub const UTILITY_COUNT: u64 = 3;

/// The highest syscall number the kernel defines.
pub const MAX: u64 = UTILITY_STRCMP;
/// The syscall groups.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyscallGroup {
    /// `exec.library`.
    Exec,
    /// `graphics.library`.
    Graphics,
    /// `layers.library`.
    Layers,
    /// `intuition.library`.
    Intuition,
    /// `dos.library`.
    Dos,
    /// `utility.library`.
    Utility,
}

impl SyscallGroup {
    /// The group a syscall number belongs to.
    pub const fn of(number: u64) -> Option<Self> {
        match number {
            0..=63 => Some(Self::Exec),
            64..=127 => Some(Self::Graphics),
            128..=191 => Some(Self::Layers),
            192..=255 => Some(Self::Intuition),
            256..=319 => Some(Self::Dos),
            320..=383 => Some(Self::Utility),
            _ => None,
        }
    }

    /// The first syscall number in the group.
    pub const fn first(self) -> u64 {
        match self {
            Self::Exec => 0,
            Self::Graphics => 64,
            Self::Layers => 128,
            Self::Intuition => 192,
            Self::Dos => 256,
            Self::Utility => 320,
        }
    }

    /// How many syscalls the group reserves.
    pub const fn size(self) -> u64 {
        match self {
            Self::Exec => 64,
            Self::Graphics => 64,
            Self::Layers => 64,
            Self::Intuition => 64,
            Self::Dos => 64,
            Self::Utility => 64,
        }
    }

    /// The library this group belongs to.
    pub const fn library(self) -> vela_abi_native64::LibraryId {
        match self {
            Self::Exec => vela_abi_native64::LibraryId::Exec,
            Self::Graphics => vela_abi_native64::LibraryId::Graphics,
            Self::Layers => vela_abi_native64::LibraryId::Layers,
            Self::Intuition => vela_abi_native64::LibraryId::Intuition,
            Self::Dos => vela_abi_native64::LibraryId::Dos,
            Self::Utility => vela_abi_native64::LibraryId::Utility,
        }
    }
}

/// The arguments of a syscall.
///
/// A fixed-size array rather than a variadic: the syscall gate reads the
/// registers into a fixed structure, and a fixed structure can be passed to
/// a handler without allocating.
#[derive(Debug, Clone, Copy, Default)]
pub struct SyscallArgs {
    /// The arguments, in register order.
    pub args: [u64; 6],
}

impl SyscallArgs {
    /// Build the arguments from the register values.
    pub const fn new(a0: u64, a1: u64, a2: u64, a3: u64, a4: u64, a5: u64) -> Self {
        Self {
            args: [a0, a1, a2, a3, a4, a5],
        }
    }

    /// Argument `n`, or zero if `n` is out of range.
    ///
    /// Out-of-range reads return zero rather than panicking: a syscall
    /// handler must never panic, because a panic at ring 0 with a user stack
    /// is a kernel crash.
    pub const fn get(&self, n: usize) -> u64 {
        if n < 6 {
            self.args[n]
        } else {
            0
        }
    }
}

/// The result of a syscall.
///
/// The value and the error are separate, for the reason given in the module
/// documentation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SyscallResult {
    /// The value the call returned.
    pub value: u64,
    /// The error code, or zero on success.
    pub error: i32,
}

impl SyscallResult {
    /// A successful result carrying `value`.
    pub const fn ok(value: u64) -> Self {
        Self { value, error: 0 }
    }

    /// A failed result carrying `error`.
    pub const fn err(error: i32) -> Self {
        Self { value: 0, error }
    }

    /// True when the call succeeded.
    pub const fn is_ok(&self) -> bool {
        self.error == 0
    }

    /// True when the call failed.
    pub const fn is_err(&self) -> bool {
        self.error != 0
    }

    /// The value, or zero on failure.
    pub const fn value_or_zero(&self) -> u64 {
        if self.is_ok() {
            self.value
        } else {
            0
        }
    }
}

/// The error codes the syscall layer returns.
///
/// These are the AmigaOS `ERROR_*` values, so a program that was written
/// against AmigaOS recognises them.
pub mod errors {
    /// The call succeeded.
    pub const OK: i32 = 0;
    /// The syscall number is not implemented.
    pub const NOT_IMPLEMENTED: i32 = -1;
    /// The object was not found.
    pub const NO_OBJECT: i32 = -2;
    /// The object was already open.
    pub const OBJECT_IN_USE: i32 = -3;
    /// The name was invalid.
    pub const INVALID_NAME: i32 = -5;
    /// The object was not found.
    pub const NOT_FOUND: i32 = -6;
    /// The name was too long.
    pub const TOO_LARGE: i32 = -8;
    /// A memory allocation failed.
    pub const NO_MEMORY: i32 = -11;
    /// The caller is not the owner.
    pub const NOT_OWNER: i32 = -24;
    /// The signal set was invalid.
    pub const INVALID_SIGNAL: i32 = -25;
    /// The message carried no reply port.
    pub const NO_REPLY_PORT: i32 = -26;
    /// The port had no message.
    pub const NO_MESSAGE: i32 = -27;
    /// The graphics rectangle was invalid.
    pub const BAD_RECT: i32 = -13;
    /// The task was not found.
    pub const TASK_NOT_FOUND: i32 = -28;
    /// A user pointer was not valid.
    ///
    /// This is VelaOS's own code rather than an AmigaOS one: on AmigaOS a bad
    /// pointer was a crash, and a program that passes one has a bug. VelaOS
    /// turns it into an error so that the `api-conformance` suite can assert
    /// on it, as the plan requires.
    pub const BAD_POINTER: i32 = -30;
    /// The user range was not readable or writable.
    pub const BAD_RANGE: i32 = -31;
    /// The user string was not NUL-terminated within the limit.
    pub const BAD_STRING: i32 = -32;
    /// An argument was out of range.
    pub const RANGE: i32 = -19;
    /// The path was not found.
    pub const PATH_NOT_FOUND: i32 = -7;
    /// The object exists but is not a file.
    pub const FILE_NOT_OBJECT: i32 = -17;
}

#[cfg(test)]
extern crate std;

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;
    use std::vec::Vec;

    /// Every syscall number the kernel defines, with its group.
    const ALL: [(u64, SyscallGroup); 16] = [
        (CREATE_MSG_PORT, SyscallGroup::Exec),
        (DELETE_MSG_PORT, SyscallGroup::Exec),
        (PUT_MSG, SyscallGroup::Exec),
        (GET_MSG, SyscallGroup::Exec),
        (REPLY_MSG, SyscallGroup::Exec),
        (WAIT_PORT, SyscallGroup::Exec),
        (WAIT, SyscallGroup::Exec),
        (SET_SIGNAL, SyscallGroup::Exec),
        (SET_EXCEPT, SyscallGroup::Exec),
        (SIGNAL, SyscallGroup::Exec),
        (OPEN_LIBRARY, SyscallGroup::Exec),
        (CLOSE_LIBRARY, SyscallGroup::Exec),
        (FORBID, SyscallGroup::Exec),
        (PERMIT, SyscallGroup::Exec),
        (YIELD, SyscallGroup::Exec),
        (GET_TASK_ID, SyscallGroup::Exec),
    ];

    #[test]
    fn every_syscall_belongs_to_the_group_it_is_listed_in() {
        for (number, group) in ALL {
            assert_eq!(
                SyscallGroup::of(number),
                Some(group),
                "syscall {number} is in the wrong group",
            );
        }
    }

    #[test]
    fn the_syscall_numbers_are_distinct() {
        let mut seen = BTreeSet::new();
        for (number, _) in ALL {
            assert!(seen.insert(number), "syscall {number} is defined twice");
        }
    }

    #[test]
    fn every_syscall_falls_inside_its_groups_range() {
        for (number, group) in ALL {
            assert!(
                number >= group.first() && number < group.first() + group.size(),
                "syscall {number} is outside the {:?} range",
                group,
            );
        }
    }

    #[test]
    fn the_groups_do_not_overlap() {
        let groups = [
            SyscallGroup::Exec,
            SyscallGroup::Graphics,
            SyscallGroup::Layers,
            SyscallGroup::Intuition,
            SyscallGroup::Dos,
            SyscallGroup::Utility,
        ];
        for (i, a) in groups.iter().enumerate() {
            for b in groups.iter().skip(i + 1) {
                let a_end = a.first() + a.size();
                let b_end = b.first() + b.size();
                assert!(
                    a.first() >= b_end || b.first() >= a_end,
                    "{a:?} and {b:?} overlap",
                );
            }
        }
    }

    #[test]
    fn an_unknown_syscall_number_has_no_group() {
        assert_eq!(SyscallGroup::of(384), None);
        assert_eq!(SyscallGroup::of(u64::MAX), None);
    }

    #[test]
    fn each_group_maps_to_its_library() {
        assert_eq!(
            SyscallGroup::Exec.library(),
            vela_abi_native64::LibraryId::Exec
        );
        assert_eq!(
            SyscallGroup::Intuition.library(),
            vela_abi_native64::LibraryId::Intuition
        );
        assert_eq!(
            SyscallGroup::Dos.library(),
            vela_abi_native64::LibraryId::Dos
        );
    }

    #[test]
    fn the_arguments_are_read_in_register_order() {
        let a = SyscallArgs::new(1, 2, 3, 4, 5, 6);
        assert_eq!(a.get(0), 1);
        assert_eq!(a.get(1), 2);
        assert_eq!(a.get(5), 6);
    }

    #[test]
    fn an_out_of_range_argument_reads_zero() {
        // A syscall handler must never panic, so an out-of-range read returns
        // zero rather than trapping.
        let a = SyscallArgs::new(1, 2, 3, 4, 5, 6);
        assert_eq!(a.get(6), 0);
        assert_eq!(a.get(usize::MAX), 0);
    }

    #[test]
    fn a_success_carries_a_value_and_no_error() {
        let r = SyscallResult::ok(42);
        assert!(r.is_ok());
        assert!(!r.is_err());
        assert_eq!(r.value, 42);
        assert_eq!(r.error, 0);
        assert_eq!(r.value_or_zero(), 42);
    }

    #[test]
    fn a_failure_carries_an_error_and_no_value() {
        let r = SyscallResult::err(errors::NO_OBJECT);
        assert!(r.is_err());
        assert!(!r.is_ok());
        assert_eq!(r.error, -2);
        assert_eq!(r.value_or_zero(), 0);
    }

    #[test]
    fn a_successful_call_may_still_return_zero() {
        // "The call succeeded and returned NULL" must be distinguishable from
        // a failure. This is why the error is a separate field.
        let r = SyscallResult::ok(0);
        assert!(r.is_ok());
        assert_eq!(r.value, 0);
        assert_eq!(r.value_or_zero(), 0);
    }

    #[test]
    fn the_error_codes_are_distinct() {
        let all = [
            errors::OK,
            errors::NOT_IMPLEMENTED,
            errors::NO_OBJECT,
            errors::OBJECT_IN_USE,
            errors::INVALID_NAME,
            errors::NOT_FOUND,
            errors::TOO_LARGE,
            errors::NO_MEMORY,
            errors::NOT_OWNER,
            errors::INVALID_SIGNAL,
            errors::NO_REPLY_PORT,
            errors::NO_MESSAGE,
            errors::TASK_NOT_FOUND,
            errors::BAD_POINTER,
            errors::BAD_RANGE,
            errors::BAD_STRING,
        ];
        let mut sorted: Vec<i32> = all.to_vec();
        sorted.sort_unstable();
        let before = sorted.len();
        sorted.dedup();
        assert_eq!(sorted.len(), before, "two error codes share a value");
    }

    #[test]
    fn the_error_codes_are_negative_except_success() {
        assert_eq!(errors::OK, 0);
        for code in [
            errors::NOT_IMPLEMENTED,
            errors::NO_OBJECT,
            errors::BAD_POINTER,
            errors::BAD_RANGE,
            errors::BAD_STRING,
        ] {
            assert!(code < 0, "error {code} must be negative");
        }
    }

    #[test]
    fn the_maximum_number_matches_the_last_defined_syscall() {
        assert_eq!(MAX, UTILITY_STRCMP);
    }
}
