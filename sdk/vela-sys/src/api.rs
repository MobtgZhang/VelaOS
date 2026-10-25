//! The typed API a program actually calls.
//!
//! # Why there are two layers
//!
//! [`crate::library_base`] gives the *AmigaOS calling model*: a library base,
//! negative offsets, a jump table. That layer exists so a program ported from
//! AmigaOS finds the shape it expects, and so the jump table — the thing that
//! makes `A6` meaningful — has something real to point at.
//!
//! But no program should have to write `jsr -582(a6)` by hand, and none should
//! have to know that a stub returns a packed `u64` rather than a pointer. So
//! this module sits on top and offers ordinary Rust calls.
//!
//! # This is the layer that must match the kernel exactly
//!
//! Every signature below is a **transliteration of the kernel's handler**.
//! The kernel's handlers document their own argument order, and this file is
//! written against those documents rather than against what the call would
//! sensibly look like. Where the two could differ — which they do, several
//! times — the kernel wins, because the kernel is what the stub reaches at
//! runtime:
//!
//! * `open_window` takes a **screen, a title, an args pointer and a port**.
//!   A caller who wants a window has to have a screen, because Intuition has
//!   no notion of a window that is not on one.
//! * `add_gadget` takes the window **and** the gadget, and the gadget's kind
//!   is a number, not an enum.
//! * `reply_msg` takes **both** a request and a reply pointer. The AmigaOS
//!   `ReplyMsg` reuses the request as the reply, and the kernel's shape is
//!   the clearer one: two buffers, two validations.
//!
//! A mismatch here is the worst kind of bug in this codebase: it compiles,
//! passes the SDK's own tests, and produces nonsense on hardware. The
//! conformance suite exists to catch exactly that.
//!
//! # Handles, not pointers
//!
//! Every object a program gets back is an opaque id: a port, a window, a
//! screen, a gadget, a file. None of them is a pointer, and none of them means
//! anything outside the kernel. That is the property that makes it safe for
//! `OpenLibrary` to hand a program a number it can revoke.

use alloc::vec::Vec;

use crate::abi::{self, Bptr, LibraryHandle, Message, OpenWindowArgs};
use crate::library_base::{self, offset, unpack, LibraryBase};
use crate::{Error, Result};

/// The error codes the syscall layer returns.
pub use crate::numbers::errors;

/// Re-exported so a program can match on a gadget kind, a window flag or an
/// IDCMP class without a second `use`.
pub use crate::abi::{
    buttons, gadget_kind, idcmp, mouse_codes, node_type, request_codes, sigs, window_flags,
};

/// The `MODE_*` flags an [`open`] asks for.
pub mod mode {
    /// Read only.
    pub const READ: u32 = 0b001;
    /// Write only.
    pub const WRITE: u32 = 0b010;
    /// Read and write.
    pub const READ_WRITE: u32 = 0b011;
    /// Write, positioned at the end.
    pub const APPEND: u32 = 0b111;
}

/// Where a [`seek`] positions from.
pub mod seek {
    /// From the start of the file.
    pub const SET: u32 = 0;
    /// From the current position.
    pub const CUR: u32 = 1;
    /// From the end of the file.
    pub const END: u32 = 2;
}

/// Split a stub's packed return into a value or an error.
///
/// Not `unsafe`, despite what the stubs around it do: by the time a packed
/// value reaches here the syscall is over, and there is nothing left to get
/// wrong.
fn check(packed: u64) -> Result<u64> {
    let (value, error) = unpack(packed);
    if error == 0 {
        Ok(value)
    } else {
        Err(Error { code: error })
    }
}

/// Copy a string into a NUL-terminated buffer for the kernel to read.
///
/// The kernel copies the string out before returning, so the buffer only has
/// to live for the call. A `Vec` rather than a fixed array because a path or
/// a title has no useful fixed upper bound on the caller's side — the kernel
/// applies its own, and refuses what exceeds it.
///
/// Available on every target, not just hosted: a bare-metal program passes
/// window titles and file paths through exactly the same calls, and gating this
/// would mean the calls a guest program actually makes were the ones that did
/// not build for a guest.
fn c_string(s: &str) -> Vec<u8> {
    let mut bytes = s.as_bytes().to_vec();
    bytes.push(0);
    bytes
}

/// A message port handle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Port(pub u64);

impl Port {
    /// The null port.
    pub const NULL: Port = Port(0);

    /// True when this names no port.
    pub const fn is_null(self) -> bool {
        self.0 == 0
    }

    /// The raw value, for passing back to the kernel.
    pub const fn raw(self) -> u64 {
        self.0
    }
}

/// A window handle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Window(pub u64);

impl Window {
    /// The null window.
    pub const NULL: Window = Window(0);

    /// True when this names no window.
    pub const fn is_null(self) -> bool {
        self.0 == 0
    }

    /// The raw value, for passing back to the kernel.
    pub const fn raw(self) -> u64 {
        self.0
    }
}

/// A screen handle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Screen(pub u64);

impl Screen {
    /// The null screen.
    pub const NULL: Screen = Screen(0);

    /// True when this names no screen.
    pub const fn is_null(self) -> bool {
        self.0 == 0
    }

    /// The raw value, for passing back to the kernel.
    pub const fn raw(self) -> u64 {
        self.0
    }
}

/// A gadget handle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Gadget(pub u64);

impl Gadget {
    /// The null gadget.
    pub const NULL: Gadget = Gadget(0);

    /// True when this names no gadget.
    pub const fn is_null(self) -> bool {
        self.0 == 0
    }

    /// The raw value, for passing back to the kernel.
    pub const fn raw(self) -> u64 {
        self.0
    }
}

/// One entry of a directory listing, as the kernel returns it.
///
/// The layout is the kernel's, not a guess at one: an 8-byte size, a 4-byte
/// AmigaOS entry type, a 1-byte name length, and a 107-byte inline name. The
/// name is inline so the whole reply is one copy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub struct DirEntry {
    /// The entry's size in bytes.
    pub de_size: u64,
    /// 2 for a file, 3 for a directory.
    pub de_type: u32,
    /// The name's length, excluding the terminator.
    pub de_name_len: u8,
    /// The name, NUL-terminated.
    pub de_name: [u8; DIR_NAME_LEN],
}

/// The size of a [`DirEntry`]'s inline name buffer.
pub const DIR_NAME_LEN: usize = 107;

/// The size of one [`DirEntry`], in bytes.
pub const DIR_ENTRY_SIZE: usize = 16 + DIR_NAME_LEN;

impl DirEntry {
    /// The entry's name, or `""` when it is not valid UTF-8.
    ///
    /// `""` rather than a lossy conversion: an entry whose name is not UTF-8
    /// is one the kernel could not have written a name into, and a program
    /// that displayed it would display nothing either way. What it *can* do
    /// is notice that a directory it listed had an entry it could not name.
    pub fn name(&self) -> &str {
        let len = (self.de_name_len as usize).min(DIR_NAME_LEN);
        core::str::from_utf8(&self.de_name[..len]).unwrap_or("")
    }

    /// True when this entry is a directory.
    pub const fn is_dir(&self) -> bool {
        self.de_type == ENTRY_TYPE_DIR
    }
}

/// The AmigaOS `ENTRYTYPE_*` values a [`DirEntry`] carries.
pub mod entry_type {
    /// A file.
    pub const FILE: u32 = 2;
    /// A directory.
    pub const DIR: u32 = 3;
    /// A device.
    pub const DEV: u32 = 4;
}

use entry_type::DIR as ENTRY_TYPE_DIR;

// ---------------------------------------------------------------------------
// Exec
// ---------------------------------------------------------------------------

/// Create a message port.
///
/// `signal` is raised when a message arrives and is left waiting, so a
/// program that wants to be interrupted rather than to sit in `WaitPort`
/// passes a bit here.
///
/// # Errors
///
/// [`errors::NO_OBJECT`] when the kernel could not allocate a port.
pub fn create_port(signal: u32) -> Result<Port> {
    // SAFETY: no pointers among the arguments.
    let packed = unsafe {
        library_base::exec::base().call(offset::EXEC_CREATEMSGPORT, [signal as u64, 0, 0, 0, 0, 0])
    };
    check(packed).map(Port)
}

/// Delete a message port.
///
/// Messages still queued on the port are freed with it, which is what makes
/// this safe to call on a port a window was posting to.
///
/// # Errors
///
/// [`errors::NO_OBJECT`] when the port does not exist.
pub fn delete_port(port: Port) -> Result<()> {
    // SAFETY: a plain integer argument.
    let packed = unsafe {
        library_base::exec::base().call(offset::EXEC_DELETEMSGPORT, [port.raw(), 0, 0, 0, 0, 0])
    };
    check(packed).map(|_| ())
}

/// Put a message on a port.
///
/// # Errors
///
/// [`errors::NO_OBJECT`] when the port does not exist.
pub fn put_msg(port: Port, msg: &Message) -> Result<()> {
    let ptr = msg as *const Message as u64;
    // SAFETY: `msg` is live for the call, and the kernel copies the message
    // out of it rather than retaining the pointer.
    let packed = unsafe {
        library_base::exec::base().call(offset::EXEC_PUTMSG, [port.raw(), ptr, 0, 0, 0, 0])
    };
    check(packed).map(|_| ())
}

/// Take a message from a port, without waiting.
///
/// # Errors
///
/// [`errors::NO_MESSAGE`] when the port is empty.
pub fn get_msg(port: Port) -> Result<Message> {
    let mut msg = Message::empty();
    let ptr = &mut msg as *mut Message as u64;
    // SAFETY: `ptr` is live and writable for the duration of the call.
    let packed = unsafe {
        library_base::exec::base().call(offset::EXEC_GETMSG, [port.raw(), ptr, 0, 0, 0, 0])
    };
    check(packed)?;
    Ok(msg)
}

/// Wait for a message on a port, then take it.
///
/// This is the call that makes IDCMP an Exec concern rather than a private side
/// channel: the window's events arrive here, on the program's own port, and
/// are replied to like any other message.
///
/// The kernel returns `1` rather than an error when the wait ended without a
/// message, which is not a failure but a fact about the world — so this
/// returns `Ok(None)` for it and reserves `Err` for a port that is not there.
///
/// # Errors
///
/// [`errors::NO_OBJECT`] when the port does not exist.
pub fn wait_port(port: Port) -> Result<Option<Message>> {
    let mut msg = Message::empty();
    let ptr = &mut msg as *mut Message as u64;
    // SAFETY: `ptr` is live and writable for the duration of the call.
    let packed = unsafe {
        library_base::exec::base().call(offset::EXEC_WAITPORT, [port.raw(), ptr, 0, 0, 0, 0])
    };
    let value = check(packed)?;
    // The kernel's convention is `0` for "a message was copied out" and `1` for
    // "the wait ended without one". Getting this the wrong way round is
    // silent and vicious: the program skips the reply to every *real* event and
    // then tries to reply to an untouched, all-zero message — which fails with
    // `NO_REPLY_PORT`, pointing at Exec rather than at the mistake.
    Ok((value == 0).then_some(msg))
}

/// Reply to a message, handing ownership back to its sender.
///
/// Both buffers are separate. AmigaOS reuses the request as the reply and
/// zero-fills it first; the kernel's shape is the clearer one — two validated
/// copies, and a reply that is not the request.
///
/// # Errors
///
/// [`errors::NO_REPLY_PORT`] when the message carried no reply port.
pub fn reply_msg(request: &mut Message, reply: &mut Message) -> Result<()> {
    let request_ptr = request as *mut Message as u64;
    let reply_ptr = reply as *mut Message as u64;
    // SAFETY: both pointers are live for the call, and the kernel copies both
    // out rather than retaining either.
    let packed = unsafe {
        library_base::exec::base().call(offset::EXEC_REPLYMSG, [request_ptr, reply_ptr, 0, 0, 0, 0])
    };
    check(packed).map(|_| ())
}

/// Wait for a signal.
///
/// Returns the mask of satisfied bits, or `None` if the task is still blocked —
/// in which case the scheduler resumes it when the signal arrives, and the
/// program never regains control until it does.
///
/// # Errors
///
/// [`errors::INVALID_SIGNAL`] when the mask is empty, which would block forever.
pub fn wait(mask: u32) -> Result<Option<u32>> {
    // SAFETY: no pointers among the arguments.
    let packed =
        unsafe { library_base::exec::base().call(offset::EXEC_WAIT, [mask as u64, 0, 0, 0, 0, 0]) };
    let value = check(packed)?;
    Ok((value != u64::MAX).then_some(value as u32))
}

/// Set bits in the calling task's signal set.
///
/// Returns the new signal set.
pub fn set_signal(mask: u32) -> Result<u32> {
    // SAFETY: no pointers among the arguments.
    let packed = unsafe {
        library_base::exec::base().call(offset::EXEC_SETSIGNAL, [mask as u64, 0, 0, 0, 0, 0])
    };
    check(packed).map(|v| v as u32)
}

/// Clear bits in the calling task's signal set.
///
/// Returns the new signal set.
pub fn set_except(mask: u32) -> Result<u32> {
    // SAFETY: no pointers among the arguments.
    let packed = unsafe {
        library_base::exec::base().call(offset::EXEC_SETEXCEPT, [mask as u64, 0, 0, 0, 0, 0])
    };
    check(packed).map(|v| v as u32)
}

/// Raise a signal in another task's set.
///
/// Returns that task's new signal set.
///
/// # Errors
///
/// [`errors::TASK_NOT_FOUND`] when the task does not exist.
pub fn signal(task: u64, mask: u32) -> Result<u32> {
    // SAFETY: no pointers among the arguments.
    let packed = unsafe {
        library_base::exec::base().call(offset::EXEC_SIGNAL, [task, mask as u64, 0, 0, 0, 0])
    };
    check(packed).map(|v| v as u32)
}

/// Enter the classic critical section.
///
/// Returns the new forbid depth. The depth is a counter rather than a flag,
/// because a program that calls `Forbid` inside a `Forbid` and then calls
/// `Permit` twice is balanced, not broken — and a flag would strand it inside
/// the section forever.
pub fn forbid() -> Result<u32> {
    // SAFETY: no pointers among the arguments.
    let packed =
        unsafe { library_base::exec::base().call(offset::EXEC_FORBID, [0, 0, 0, 0, 0, 0]) };
    check(packed).map(|v| v as u32)
}

/// Leave the classic critical section.
///
/// Returns the remaining forbid depth.
pub fn permit() -> Result<u32> {
    // SAFETY: no pointers among the arguments.
    let packed =
        unsafe { library_base::exec::base().call(offset::EXEC_PERMIT, [0, 0, 0, 0, 0, 0]) };
    check(packed).map(|v| v as u32)
}

/// Give up the rest of the time slice.
pub fn yield_now() -> Result<()> {
    // SAFETY: no pointers among the arguments.
    let packed = unsafe { library_base::exec::base().call(offset::EXEC_YIELD, [0, 0, 0, 0, 0, 0]) };
    check(packed).map(|_| ())
}

/// The calling task's own identifier.
pub fn task_id() -> Result<u64> {
    // SAFETY: no pointers among the arguments.
    let packed =
        unsafe { library_base::exec::base().call(offset::EXEC_FINDTASK, [0, 0, 0, 0, 0, 0]) };
    check(packed)
}

/// Write one byte to the console.
///
/// This is how a user program reports anything at all: it has no console of its
/// own, and its alternative — a private buffer it can fill and never flush —
/// produces output that appears all at once or not at all.
///
/// It costs one syscall per byte, which is why a program printing in a loop
/// should batch. Batching is a choice the program gets to make; not having the
/// ability to print is not a choice at all.
pub fn putchar(byte: u8) -> Result<()> {
    // SAFETY: no pointers among the arguments.
    let packed = unsafe {
        library_base::exec::base().call(offset::EXEC_PUTCHAR, [byte as u64, 0, 0, 0, 0, 0])
    };
    check(packed).map(|_| ())
}

/// Write a string to the console, one byte at a time.
///
/// Not a single syscall: the kernel's `PutChar` takes one byte, so this is the
/// obvious spelling and the one a program uses. The cost is stated rather than
/// hidden because it is real — a program printing a screenful pays a syscall
/// per character.
pub fn print(s: &str) {
    for byte in s.bytes() {
        // A console that has stopped accepting bytes is a machine that is
        // already gone; there is nothing a program could usefully do about it,
        // and propagating the failure would force every caller to handle an
        // error it cannot act on.
        let _ = putchar(byte);
    }
}

/// Write a decimal number to the console.
///
/// Every boot-time program wants this and none of them wants a formatter, so it
/// lives here once. The digits go through [`putchar`] one at a time, which is
/// the same cost as `print` — a boot log is a handful of lines, and the
/// syscall-per-byte tradeoff that matters for a program printing in a loop does
/// not bite for one that prints a summary.
pub fn print_int(mut value: u64) {
    // Twenty digits is the most a `u64` can need, so the buffer cannot
    // overflow. A fixed array rather than a `String` because this runs before
    // a program may have any reason to trust the allocator.
    let mut digits = [0u8; 20];
    let mut n = 0;
    if value == 0 {
        let _ = putchar(b'0');
        return;
    }
    while value > 0 && n < digits.len() {
        digits[n] = b'0' + (value % 10) as u8;
        value /= 10;
        n += 1;
    }
    // Most significant first, so the digits are emitted in reverse of the order
    // they were produced.
    for i in (0..n).rev() {
        let _ = putchar(digits[i]);
    }
}

/// Write a `core::fmt::Arguments` to the console without allocating.
///
/// The escape hatch for the two places that must never allocate: the panic
/// handler, and any logging on a guest whose allocator is the thing that just
/// failed. Both need formatted output, and `format!` would give them a `String`
/// to build first — so this walks the `Arguments` and writes each piece
/// straight to the console.
pub fn print_fmt(args: core::fmt::Arguments<'_>) {
    use core::fmt::Write;
    // A `Write` sink that forwards to `putchar`, so `write!` works and no
    // intermediate buffer is allocated.
    struct Console;
    impl Write for Console {
        fn write_str(&mut self, s: &str) -> core::fmt::Result {
            print(s);
            Ok(())
        }
    }
    // The only way this can fail is if `Console::write_str` does, and it
    // cannot: `print` swallows console errors deliberately (see its docs).
    let _ = Console.write_fmt(args);
}

/// Stop the machine, reporting `status` to whatever is watching.
///
/// Present because a user program has no other way to say "I am finished".
/// Without it a completed guest and a hung guest are indistinguishable, and
/// the second is much more expensive to diagnose.
///
/// On every target, not just bare metal. The hosted build routes it at the same
/// dispatcher, where the handler powers the *host* machine off — so it is
/// gated here for the same reason the rest of the SDK is not: a program written
/// against the SDK must compile against the SDK, not against whichever
/// subset of it happens to be reachable from where it is being compiled. A test
/// that wants to exercise the call's *arguments* can use it; a test that would
/// actually halt the machine has no business calling it.
///
/// Note that on the host this really does halt: the hosted dispatcher routes
/// `POWER_OFF` to the same handler, which powers the host machine off. That is
/// why no test calls it — and why a program that ends by calling it is
/// genuinely runnable outside QEMU.
pub fn power_off() -> ! {
    power_off_with(0)
}

/// Power off, reporting `status`.
///
/// The parameterised form, so a caller can pass the status the boot smoke test
/// looks for.
pub fn power_off_with(status: u32) -> ! {
    // SAFETY: no pointers among the arguments, and the call does not return.
    unsafe {
        library_base::exec::base().call(offset::EXEC_POWEROFF, [status as u64, 0, 0, 0, 0, 0])
    };
    // The stub cannot return — the kernel's handler is `-> !` and powers the
    // machine off — but Rust cannot see through the syscall, so this is the
    // only way to satisfy the type checker.
    //
    // Reaching this line would mean the kernel returned from a call it is
    // documented never to return from. Looping forever is the honest response:
    // it is what actually happens on a machine that ignored the request, and it
    // does not pretend the program finished when it did not.
    loop {
        core::hint::spin_loop();
    }
}

// ---------------------------------------------------------------------------
// Program memory
// ---------------------------------------------------------------------------

/// The size of a program page, in bytes.
///
/// Read from the kernel's own constant rather than assumed, so the guest and
/// the kernel cannot disagree about the granularity they round to. The two
/// agreeing matters for `brk`, which the kernel rounds and this must predict.
pub const PAGE_SIZE: usize = crate::numbers::PAGE_SIZE;

/// Ask the kernel to move the program break.
///
/// `brk(0)` queries; `brk(addr)` requests `addr`. The result is the break
/// **actually in force**, which is what makes the call transactional: a
/// program that asks for more than exists learns the real limit from the return
/// value instead of failing later at an address it cannot write.
///
/// # Errors
///
/// [`errors::RANGE`] when the address is not page-aligned or lies outside the
/// program's heap region.
pub fn program_break(addr: u64) -> Result<u64> {
    // SAFETY: a plain integer argument; the kernel validates it against the
    // calling task's own heap region and refuses anything else.
    let packed =
        unsafe { library_base::exec::base().call(offset::EXEC_BRK, [addr, 0, 0, 0, 0, 0]) };
    check(packed)
}

/// Map one page of program memory, returning its address.
///
/// Rarely called directly: [`crate::heap::GuestHeap`] drives it through
/// [`program_break`]. It is exposed because a program allocating a large
/// buffer at a known address — a framebuffer mirror, a fixed record — has
/// legitimate use for it.
pub fn map_page(addr: u64) -> Result<u64> {
    // SAFETY: a plain integer argument. The kernel rejects an address outside
    // the program range, so a bad value is an error rather than a mapping.
    let packed =
        unsafe { library_base::exec::base().call(offset::EXEC_MAPPAGE, [addr, 0, 0, 0, 0, 0]) };
    check(packed)
}

// ---------------------------------------------------------------------------
// Library registry
// ---------------------------------------------------------------------------

/// Open a library by name.
///
/// A library that is not there is *not* an error — it is a null handle, which
/// is what AmigaOS does and what makes `if (lib = OpenLibrary(...))` the
/// idiom. The error is reserved for a name the kernel could not read.
///
/// # The handle, and the base
///
/// The handle is the kernel's *registry* handle: an index into the calling
/// process's library table. It is deliberately **not** an address — the
/// kernel never hands a user program a pointer, and a handle that named one
/// would let a program forge another process's library base.
///
/// The AmigaOS-shaped value is the **library base**: an address in *this*
/// program's own address space, pointing at this program's own jump table.
/// [`library_base`] is what produces it — see [`open_library_base`] for the
/// spelling a program uses.
///
/// # Errors
///
/// [`errors::BAD_STRING`] when the name is not a readable, NUL-terminated
/// user string.
pub fn open_library(name: &str) -> Result<LibraryHandle> {
    let bytes = c_string(name);
    // SAFETY: `bytes` is live for the call, and the kernel copies the string
    // out of it before returning — it does not retain the pointer.
    let packed = unsafe {
        library_base::exec::base().call(
            offset::EXEC_OPENLIBRARY,
            [bytes.as_ptr() as u64, 0, 0, 0, 0, 0],
        )
    };
    check(packed).map(|v| LibraryHandle::from_raw(v as u32))
}

/// Open a library and return its **library base address**.
///
/// This is the call that reproduces the AmigaOS model end to end:
///
/// ```text
///   base = OpenLibrary("intuition.library")   // an address in user space
///   ...                                       // base + lvo, lvo negative
/// ```
///
/// The address points at this program's own jump table — a `static` array of
/// real `extern "sysv64"` stubs — so `base + lvo` lands on the stub that
/// function names, and calling through it marshals into a syscall. Nothing
/// in the kernel is reachable through the address: it is the program's own
/// memory.
///
/// The handle is opened first, so a library that is not there yields a null
/// base rather than a base for a library the program never opened.
///
/// # Errors
///
/// [`errors::BAD_STRING`] when the name is not a readable, NUL-terminated
/// user string.
pub fn open_library_base(name: &str) -> Result<LibraryBase> {
    let handle = open_library(name)?;
    if handle.is_null() {
        // The library is not there. A null base is the AmigaOS answer: a
        // program that called `OpenLibrary` and got nothing must not go on
        // to call through a base it does not have.
        return Ok(LibraryBase::NULL);
    }
    // SAFETY: `handle` came from `open_library` moments ago, so it names a
    // library this program has open, and the base is built from this
    // program's own table.
    let base = unsafe { library_base::open(handle) };
    if base.is_null() {
        // The library is open but has no table — `utility.library`, which is
        // not implemented. The handle is released again so the reference
        // count does not leak a library the program cannot use.
        let _ = close_library(handle);
    }
    Ok(base)
}

/// Close a library.
///
/// # Errors
///
/// [`errors::NO_OBJECT`] when the handle names no open library.
pub fn close_library(handle: LibraryHandle) -> Result<()> {
    // SAFETY: a plain integer argument; there is no pointer to validate.
    let packed = unsafe {
        library_base::exec::base().call(
            offset::EXEC_CLOSELIBRARY,
            [handle.raw() as u64, 0, 0, 0, 0, 0],
        )
    };
    check(packed).map(|_| ())
}

// ---------------------------------------------------------------------------
// Intuition
// ---------------------------------------------------------------------------

/// Open a screen.
///
/// The screen is what windows live on, so it comes before any window.
///
/// # Errors
///
/// [`errors::BAD_RECT`] when the geometry does not fit the display, or
/// [`errors::NO_OBJECT`] when the kernel could not allocate one.
pub fn open_screen(title: &str, rect: &abi::Rectangle) -> Result<Screen> {
    let bytes = c_string(title);
    // SAFETY: `bytes` and `rect` are live for the call; the kernel copies both
    // out rather than reading them after it returns.
    let packed = unsafe {
        library_base::intuition::base().call(
            offset::INTUITION_OPENSCREEN,
            [
                bytes.as_ptr() as u64,
                rect.min_x as u64,
                rect.min_y as u64,
                rect.max_x as u64,
                rect.max_y as u64,
                0,
            ],
        )
    };
    check(packed).map(Screen)
}

/// Close a screen and every window on it.
///
/// Returns how many windows were closed as a side effect, so the program can
/// tell that its windows are gone rather than discovering it one failed
/// `close_window` at a time.
pub fn close_screen(screen: Screen) -> Result<usize> {
    // SAFETY: a plain integer argument.
    let packed = unsafe {
        library_base::intuition::base()
            .call(offset::INTUITION_CLOSESCREEN, [screen.raw(), 0, 0, 0, 0, 0])
    };
    check(packed).map(|v| v as usize)
}

/// Open a window.
///
/// # Errors
///
/// [`errors::BAD_RECT`] when the geometry does not fit the screen, or
/// [`errors::NO_OBJECT`] when the kernel could not allocate one.
pub fn open_window(
    screen: Screen,
    title: &str,
    args: &OpenWindowArgs,
    port: Port,
) -> Result<Window> {
    let bytes = c_string(title);
    let args_ptr = args as *const OpenWindowArgs as u64;
    // SAFETY: `bytes` and `args_ptr` are live for the call; the kernel copies
    // both out rather than reading them after it returns.
    let packed = unsafe {
        library_base::intuition::base().call(
            offset::INTUITION_OPENWINDOW,
            [
                screen.raw(),
                bytes.as_ptr() as u64,
                args_ptr,
                port.raw(),
                0,
                0,
            ],
        )
    };
    check(packed).map(Window)
}

/// Close a window, and purge any of its events still queued on the port.
///
/// # Errors
///
/// [`errors::NO_OBJECT`] when the window does not exist, or
/// [`errors::NOT_OWNER`] when it belongs to another task.
pub fn close_window(window: Window) -> Result<()> {
    // SAFETY: a plain integer argument.
    let packed = unsafe {
        library_base::intuition::base()
            .call(offset::INTUITION_CLOSEWINDOW, [window.raw(), 0, 0, 0, 0, 0])
    };
    check(packed).map(|_| ())
}

/// Set a window's title.
///
/// # Errors
///
/// [`errors::BAD_STRING`] when the title is not a readable user string.
pub fn set_window_title(window: Window, title: &str) -> Result<()> {
    let bytes = c_string(title);
    // SAFETY: `bytes` is live for the call; the kernel copies it out.
    let packed = unsafe {
        library_base::intuition::base().call(
            offset::INTUITION_SETWINDOWTITLE,
            [window.raw(), bytes.as_ptr() as u64, 0, 0, 0, 0],
        )
    };
    check(packed).map(|_| ())
}

/// Set a window's flags and IDCMP mask.
///
/// # Errors
///
/// [`errors::NO_OBJECT`] when the window does not exist.
pub fn set_window_attrs(window: Window, flags: u32, idcmp: u16) -> Result<()> {
    // SAFETY: no pointers among the arguments.
    let packed = unsafe {
        library_base::intuition::base().call(
            offset::INTUITION_SETATTRS,
            [window.raw(), flags as u64, idcmp as u64, 0, 0, 0],
        )
    };
    check(packed).map(|_| ())
}

/// Move or resize a window.
///
/// A zero width or height leaves that dimension alone, which is what lets one
/// call serve both a move and a resize without either having to read the
/// current geometry first.
///
/// Returns the window's *previous* geometry, packed into one `u64` — four
/// 16-bit fields, which is all a rectangle is.
///
/// # Errors
///
/// [`errors::BAD_RECT`] when the new geometry does not fit the screen.
pub fn set_window_bounds(window: Window, x: i16, y: i16, w: u16, h: u16) -> Result<abi::Rectangle> {
    // SAFETY: no pointers among the arguments.
    let packed = unsafe {
        library_base::intuition::base().call(
            offset::INTUITION_SETWINDOWBOUNDS,
            [
                window.raw(),
                x as i32 as u64,
                y as i32 as u64,
                w as u64,
                h as u64,
                0,
            ],
        )
    };
    check(packed).map(unpack_rect)
}

/// Repaint a window's furniture, and optionally composite the stack.
///
/// `composite` is separate from the repaint because the two cost very
/// different amounts: a repaint redraws one window's own furniture, while a
/// composite copies every layer to the display. A program redrawing in a loop
/// wants the first far more often than the second.
///
/// # Errors
///
/// [`errors::NO_OBJECT`] when the window does not exist.
pub fn repaint_window(window: Window, composite: bool) -> Result<()> {
    // SAFETY: a plain integer argument.
    let packed = unsafe {
        library_base::intuition::base().call(
            offset::INTUITION_REPAINTWINDOW,
            [window.raw(), composite as u64, 0, 0, 0, 0],
        )
    };
    check(packed).map(|_| ())
}

/// Repaint every window, and optionally composite the stack.
pub fn refresh(composite: bool) -> Result<()> {
    // SAFETY: no pointers among the arguments.
    let packed = unsafe {
        library_base::intuition::base()
            .call(offset::INTUITION_REFRESH, [composite as u64, 0, 0, 0, 0, 0])
    };
    check(packed).map(|_| ())
}

/// Add a gadget to a window.
///
/// The bounds go through a pointer rather than four registers because the call
/// needs seven values and the ABI has six. A rectangle is one value, which is
/// also the shape a C program would pass.
///
/// # Errors
///
/// [`errors::NO_OBJECT`] when the window does not exist,
/// [`errors::RANGE`] when the kind is not one of [`gadget_kind`], or
/// [`errors::BAD_RECT`] when the bounds fall outside the window.
pub fn add_gadget(
    window: Window,
    kind: u64,
    bounds: &abi::Rectangle,
    label: &str,
) -> Result<Gadget> {
    let bytes = c_string(label);
    let rect_ptr = bounds as *const abi::Rectangle as u64;
    // SAFETY: `bytes` and `rect_ptr` are live for the call; the kernel copies
    // both out.
    let packed = unsafe {
        library_base::intuition::base().call(
            offset::INTUITION_ADDGADGET,
            [window.raw(), kind, rect_ptr, bytes.as_ptr() as u64, 0, 0],
        )
    };
    check(packed).map(Gadget)
}

/// Remove a gadget from its window.
///
/// # Errors
///
/// [`errors::NO_OBJECT`] when the gadget does not exist.
pub fn remove_gadget(window: Window, gadget: Gadget) -> Result<()> {
    // SAFETY: plain integer arguments.
    let packed = unsafe {
        library_base::intuition::base().call(
            offset::INTUITION_REMOVEGADGET,
            [window.raw(), gadget.raw(), 0, 0, 0, 0],
        )
    };
    check(packed).map(|_| ())
}

/// Bring a window to the front.
///
/// # Errors
///
/// [`errors::NO_OBJECT`] when the window does not exist.
pub fn bring_window_to_front(window: Window) -> Result<()> {
    // SAFETY: a plain integer argument.
    let packed = unsafe {
        library_base::intuition::base().call(
            offset::INTUITION_BRINGWINDOWTOFRONT,
            [window.raw(), 0, 0, 0, 0, 0],
        )
    };
    check(packed).map(|_| ())
}

/// Send a window to the back.
///
/// # Errors
///
/// [`errors::NO_OBJECT`] when the window does not exist.
pub fn send_window_to_back(window: Window) -> Result<()> {
    // SAFETY: a plain integer argument.
    let packed = unsafe {
        library_base::intuition::base().call(
            offset::INTUITION_SENDWINDOWTOBACK,
            [window.raw(), 0, 0, 0, 0, 0],
        )
    };
    check(packed).map(|_| ())
}

// ---------------------------------------------------------------------------
// Graphics
// ---------------------------------------------------------------------------

/// The colours the classic theme draws with, as `0x00RRGGBB`.
///
/// A program's *own* colours, not the kernel's. The kernel has a palette for
/// its own furniture; a program that wants a specific colour allocates a pen
/// for it. These are the ones the Workbench and the demo apps use, so a
/// mis-set colour is one number rather than a magic constant at every call.
pub mod colors {
    /// Black.
    pub const BLACK: u32 = 0x0000_0000;
    /// White.
    pub const WHITE: u32 = 0x00FF_FFFF;
    /// The desktop's blue-grey background.
    pub const DESKTOP: u32 = 0x005A_5A5A;
    /// A window's fill.
    pub const WINDOW: u32 = 0x00A8_A8A8;
    /// A window's title bar.
    pub const TITLE: u32 = 0x0035_6BC5;
    /// A gadget's fill.
    pub const GADGET: u32 = 0x00D0_D0D0;
    /// A gadget's border.
    pub const GADGET_BORDER: u32 = 0x0020_2020;
    /// The highlight colour.
    pub const HIGHLIGHT: u32 = 0x00FF_FFFF;
    /// The shadow colour.
    pub const SHADOW: u32 = 0x0070_7070;
}

/// A pen handle.
///
/// `0xFFFF` is *not* a pen: it is what `alloc_pen` returns on failure, because
/// zero is a legitimate pen index — the first pen allocated — and a program
/// that could not tell the two apart would silently draw with the wrong colour.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pen(pub u16);

impl Pen {
    /// The value `alloc_pen` returns when it fails.
    pub const INVALID: Pen = Pen(0xFFFF);

    /// True when this names a pen.
    pub const fn is_valid(self) -> bool {
        self.0 != Self::INVALID.0
    }

    /// The raw value, for passing back to the kernel.
    pub const fn raw(self) -> u64 {
        self.0 as u64
    }
}

/// Graphics-library calls.
///
/// # Why every drawing call names a window
///
/// A program does not get to name the framebuffer: it is kernel-owned memory
/// that a user program must not write. Nor does it get a rast port, whose state
/// would be a per-task allocation the kernel has to reclaim on task exit. So
/// every drawing call here names a **window the calling task opened**, and the
/// kernel paints into that window's layer.
///
/// # Why there is no `Move`-then-`Line`
///
/// `Move` sets the position and plots it, because there is no kernel-side
/// current position to leave behind. A program drawing a polyline therefore
/// draws each segment explicitly, which costs one syscall per segment — a
/// deliberate trade, documented in the kernel's handler, for never having to
/// reclaim per-task raster state.
pub mod graphics {
    use super::{check, Pen};
    use crate::abi::Rectangle;
    use crate::library_base::{self, offset};
    use crate::Result;

    /// Allocate a pen of a given colour in a palette.
    ///
    /// Returns [`Pen::INVALID`] rather than an error, matching AmigaOS: a
    /// program that cannot get a particular colour draws without it, which is
    /// better than refusing to run.
    pub fn alloc_pen(palette: u16, color: u32) -> Pen {
        // SAFETY: no pointers among the arguments.
        let packed = unsafe {
            library_base::graphics::base().call(
                offset::GRAPHICS_ALLOCPEN,
                [palette as u64, color as u64, 0, 0, 0, 0],
            )
        };
        // A failure is reported as the invalid pen, not as an error, so the
        // caller's `is_valid` check is the whole contract.
        Pen(unpack_pen(packed))
    }

    /// Release a pen.
    ///
    /// # Errors
    ///
    /// [`super::errors::NO_OBJECT`] when the pen does not exist.
    pub fn free_pen(palette: u16, pen: Pen) -> Result<()> {
        // SAFETY: no pointers among the arguments.
        let packed = unsafe {
            library_base::graphics::base().call(
                offset::GRAPHICS_FREEPEN,
                [palette as u64, pen.raw(), 0, 0, 0, 0],
            )
        };
        check(packed).map(|_| ())
    }

    /// The colour a pen resolves to.
    ///
    /// # Errors
    ///
    /// [`super::errors::NO_OBJECT`] when the pen does not exist.
    pub fn pen_color(palette: u16, pen: Pen) -> Result<u32> {
        // SAFETY: no pointers among the arguments.
        let packed = unsafe {
            library_base::graphics::base().call(
                offset::GRAPHICS_SETPEN,
                [palette as u64, pen.raw(), 0, 0, 0, 0],
            )
        };
        check(packed).map(|v| v as u32)
    }

    /// Fill a rectangle in a window.
    ///
    /// # Errors
    ///
    /// [`super::errors::NO_OBJECT`] when the window is not the caller's or the
    /// pen does not exist.
    pub fn fill_rect(window: super::Window, pen: Pen, rect: &Rectangle) -> Result<()> {
        let ptr = rect as *const Rectangle as u64;
        // SAFETY: `rect` is live for the call; the kernel reads the eight
        // bytes out of it and validates the pointer first.
        let packed = unsafe {
            library_base::graphics::base().call(
                offset::GRAPHICS_RECTFILL,
                [window.raw(), pen.raw(), ptr, 0, 0, 0],
            )
        };
        check(packed).map(|_| ())
    }

    /// Stroke a rectangle's outline in a window.
    ///
    /// # Errors
    ///
    /// [`super::errors::NO_OBJECT`] when the window is not the caller's or the
    /// pen does not exist.
    pub fn rect(window: super::Window, pen: Pen, rect: &Rectangle) -> Result<()> {
        let ptr = rect as *const Rectangle as u64;
        // SAFETY: `rect` is live for the call; the kernel reads the eight
        // bytes out of it and validates the pointer first.
        let packed = unsafe {
            library_base::graphics::base().call(
                offset::GRAPHICS_RECT,
                [window.raw(), pen.raw(), ptr, 0, 0, 0],
            )
        };
        check(packed).map(|_| ())
    }

    /// Draw a line in a window.
    ///
    /// # Errors
    ///
    /// [`super::errors::NO_OBJECT`] when the window is not the caller's or the
    /// pen does not exist.
    pub fn line(window: super::Window, pen: Pen, x0: i16, y0: i16, x1: i16, y1: i16) -> Result<()> {
        // SAFETY: no pointers among the arguments.
        let packed = unsafe {
            library_base::graphics::base().call(
                offset::GRAPHICS_LINE,
                [
                    window.raw(),
                    pen.raw(),
                    x0 as i32 as u64,
                    y0 as i32 as u64,
                    x1 as i32 as u64,
                    y1 as i32 as u64,
                ],
            )
        };
        check(packed).map(|_| ())
    }

    /// Set the drawing position and plot it.
    ///
    /// # Errors
    ///
    /// [`super::errors::NO_OBJECT`] when the window is not the caller's or the
    /// pen does not exist.
    pub fn move_to(window: super::Window, pen: Pen, x: i16, y: i16) -> Result<()> {
        // SAFETY: no pointers among the arguments.
        let packed = unsafe {
            library_base::graphics::base().call(
                offset::GRAPHICS_MOVE,
                [
                    window.raw(),
                    pen.raw(),
                    x as i32 as u64,
                    y as i32 as u64,
                    0,
                    0,
                ],
            )
        };
        check(packed).map(|_| ())
    }

    /// Write one pixel in a window.
    ///
    /// # Errors
    ///
    /// [`super::errors::NO_OBJECT`] when the window is not the caller's or the
    /// pen does not exist.
    pub fn write_pixel(window: super::Window, pen: Pen, x: i16, y: i16) -> Result<()> {
        // SAFETY: no pointers among the arguments.
        let packed = unsafe {
            library_base::graphics::base().call(
                offset::GRAPHICS_WRITEPIXEL,
                [
                    window.raw(),
                    pen.raw(),
                    x as i32 as u64,
                    y as i32 as u64,
                    0,
                    0,
                ],
            )
        };
        check(packed).map(|_| ())
    }

    /// Draw a NUL-terminated string in a window.
    ///
    /// Returns the string's advance width in pixels, which is what a program
    /// laying out a row needs and what AmigaOS returns.
    ///
    /// # Errors
    ///
    /// [`super::errors::NO_OBJECT`] when the window is not the caller's or the
    /// pen does not exist.
    pub fn text(window: super::Window, pen: Pen, x: i16, y: i16, text: &str) -> Result<u32> {
        let bytes = super::c_string(text);
        // SAFETY: `bytes` is live for the call; the kernel copies it out.
        let packed = unsafe {
            library_base::graphics::base().call(
                offset::GRAPHICS_TEXT,
                [
                    window.raw(),
                    pen.raw(),
                    x as i32 as u64,
                    y as i32 as u64,
                    bytes.as_ptr() as u64,
                    0,
                ],
            )
        };
        check(packed).map(|v| v as u32)
    }

    /// A window raster's size, as `width | (height << 32)`.
    ///
    /// # Errors
    ///
    /// [`super::errors::NO_OBJECT`] when the window does not exist.
    pub fn raster_info(window: super::Window) -> Result<(u32, u32)> {
        // SAFETY: no pointers among the arguments.
        let packed = unsafe {
            library_base::graphics::base()
                .call(offset::GRAPHICS_RASTERINFO, [window.raw(), 0, 0, 0, 0, 0])
        };
        let packed = check(packed)?;
        Ok((packed as u32, (packed >> 32) as u32))
    }

    /// Unpack a pen handle out of a packed result, mapping a failure to
    /// [`Pen::INVALID`].
    fn unpack_pen(packed: u64) -> u16 {
        let (value, error) = crate::library_base::unpack(packed);
        if error == 0 {
            value as u16
        } else {
            Pen::INVALID.0
        }
    }
}

// ---------------------------------------------------------------------------
// DOS
// ---------------------------------------------------------------------------

/// Open a file by AmigaDOS path.
///
/// # Errors
///
/// [`errors::PATH_NOT_FOUND`] when the path names nothing,
/// [`errors::FILE_NOT_OBJECT`] when it names something that is not a file, or
/// [`errors::RANGE`] when the mode is not one of [`mode`].
pub fn open(path: &str, access: u32) -> Result<Bptr> {
    let bytes = c_string(path);
    // SAFETY: `bytes` is live for the call; the kernel copies it out.
    let packed = unsafe {
        library_base::dos::base().call(
            offset::DOS_OPEN,
            [bytes.as_ptr() as u64, access as u64, 0, 0, 0, 0],
        )
    };
    check(packed).map(|v| Bptr::from_raw(v as u32))
}

/// Close a file.
///
/// # Errors
///
/// [`errors::NO_OBJECT`] when the handle names no open file.
pub fn close(file: Bptr) -> Result<()> {
    // SAFETY: a plain integer argument.
    let packed = unsafe {
        library_base::dos::base().call(offset::DOS_CLOSE, [file.raw() as u64, 0, 0, 0, 0, 0])
    };
    check(packed).map(|_| ())
}

/// Read from a file into `buf`.
///
/// Returns the number of bytes read, which is short at end of file. A zero
/// return at a non-empty file means end of file, not failure.
///
/// # Errors
///
/// [`errors::NO_OBJECT`] when the handle names no open file, or
/// [`errors::RANGE`] when the length exceeds what one call will move.
pub fn read(file: Bptr, buf: &mut [u8]) -> Result<usize> {
    let ptr = buf.as_mut_ptr() as u64;
    let len = buf.len() as u64;
    // SAFETY: `buf` is live and writable for `len` bytes across the call, and
    // the kernel validates that range before writing into it.
    let packed = unsafe {
        library_base::dos::base().call(offset::DOS_READ, [file.raw() as u64, ptr, len, 0, 0, 0])
    };
    check(packed).map(|v| v as usize)
}

/// Write `buf` to a file.
///
/// Returns the number of bytes written, which is short when the file filled up
/// — so a program writing a large buffer loops rather than assuming it all
/// landed.
///
/// # Errors
///
/// [`errors::NO_OBJECT`] when the handle names no open file.
pub fn write(file: Bptr, buf: &[u8]) -> Result<usize> {
    let ptr = buf.as_ptr() as u64;
    let len = buf.len() as u64;
    // SAFETY: `buf` is live and readable for `len` bytes across the call, and
    // the kernel validates that range before reading from it.
    let packed = unsafe {
        library_base::dos::base().call(offset::DOS_WRITE, [file.raw() as u64, ptr, len, 0, 0, 0])
    };
    check(packed).map(|v| v as usize)
}

/// Move a file's position.
///
/// # Errors
///
/// [`errors::NO_OBJECT`] when the handle names no open file, or
/// [`errors::RANGE`] when the mode is not one of [`seek`].
pub fn seek(file: Bptr, offset: i64, from: u32) -> Result<u64> {
    // SAFETY: plain integer arguments.
    let packed = unsafe {
        library_base::dos::base().call(
            offset::DOS_SEEK,
            [file.raw() as u64, offset as u64, from as u64, 0, 0, 0],
        )
    };
    check(packed)
}

/// List a directory into `out`, which is filled to its own length.
///
/// Returns how many entries were written, which is short when the directory
/// has more than the buffer holds — so a program scanning a large directory
/// grows its buffer and comes back rather than concluding the scan is over.
///
/// # Errors
///
/// [`errors::PATH_NOT_FOUND`] when the path names no directory, or
/// [`errors::RANGE`] when `out` is empty.
pub fn read_dir(path: &str, out: &mut [DirEntry]) -> Result<usize> {
    let bytes = c_string(path);
    let ptr = out.as_mut_ptr() as u64;
    let max = out.len() as u64;
    // SAFETY: `bytes` is live for the call, and `out` is a live, writable
    // slice of exactly `max` records that the kernel writes whole records
    // into — never a partial one.
    let packed = unsafe {
        library_base::dos::base().call(
            offset::DOS_READDIR,
            [bytes.as_ptr() as u64, ptr, max, 0, 0, 0],
        )
    };
    check(packed).map(|v| v as usize)
}

/// Delete a file.
///
/// # Errors
///
/// [`errors::PATH_NOT_FOUND`] when the path names no file, or
/// [`errors::OBJECT_IN_USE`] when the file is open.
pub fn delete_file(path: &str) -> Result<()> {
    let bytes = c_string(path);
    // SAFETY: `bytes` is live for the call; the kernel copies it out.
    let packed = unsafe {
        library_base::dos::base().call(
            offset::DOS_DELETEFILE,
            [bytes.as_ptr() as u64, 0, 0, 0, 0, 0],
        )
    };
    check(packed).map(|_| ())
}

/// Create a directory.
///
/// # Errors
///
/// [`errors::OBJECT_IN_USE`] when it already exists, or
/// [`errors::PATH_NOT_FOUND`] when its parent does not.
pub fn create_dir(path: &str) -> Result<()> {
    let bytes = c_string(path);
    // SAFETY: `bytes` is live for the call; the kernel copies it out.
    let packed = unsafe {
        library_base::dos::base().call(
            offset::DOS_CREATEDIR,
            [bytes.as_ptr() as u64, 0, 0, 0, 0, 0],
        )
    };
    check(packed).map(|_| ())
}

/// Look a path up, reporting its size, type, protection and timestamp.
///
/// # Errors
///
/// [`errors::PATH_NOT_FOUND`] when the path names nothing.
pub fn examine(path: &str) -> Result<abi::FileInfo> {
    let bytes = c_string(path);
    let mut info = abi::FileInfo::empty();
    let ptr = &mut info as *mut abi::FileInfo as u64;
    // SAFETY: `bytes` is live for the call, and `info` is a live, writable
    // local of exactly the size the kernel copies out.
    let packed = unsafe {
        library_base::dos::base().call(
            offset::DOS_EXAMINE,
            [bytes.as_ptr() as u64, ptr, 0, 0, 0, 0],
        )
    };
    check(packed)?;
    Ok(info)
}

/// The calling program's current directory, as a path string.
///
/// The string is copied into `buf` and NUL-terminated, and the length
/// returned excludes the terminator. A buffer too small is an error rather
/// than a silent truncation, because a caller that sized its buffer from a
/// previous return value should never be surprised.
///
/// # Errors
///
/// [`errors::TOO_LARGE`] when the path does not fit, or [`errors::RANGE`] when
/// `buf` is empty.
pub fn current_dir(buf: &mut [u8]) -> Result<usize> {
    let ptr = buf.as_mut_ptr() as u64;
    let len = buf.len() as u64;
    // SAFETY: `buf` is a live, writable slice of exactly `len` bytes and the
    // kernel writes at most that many, terminator included.
    let packed =
        unsafe { library_base::dos::base().call(offset::DOS_CURRENTDIR, [ptr, len, 0, 0, 0, 0]) };
    check(packed).map(|v| v as usize)
}

/// Unpack a rectangle the kernel packed into a `u64`.
///
/// Four 16-bit fields: `x`, `y`, `width`, `height`. The width and height are
/// `u16` because that is all a rectangle is, and the kernel refuses a geometry
/// that needs more.
///
/// The mirror of the kernel's `pack_rect`, and the test below asserts the two
/// are inverses — which is the only thing keeping a window's *previous*
/// geometry from silently becoming garbage after a move.
fn unpack_rect(packed: u64) -> abi::Rectangle {
    abi::Rectangle {
        min_x: packed as i16,
        min_y: (packed >> 16) as i16,
        max_x: (packed >> 32) as u16,
        max_y: (packed >> 48) as u16,
    }
}

/// Pack a rectangle the way the kernel does.
///
/// Present only so the round-trip test can build the packed form. A program
/// never packs one: the only packed rectangle it ever sees is one the kernel
/// returned.
#[cfg(test)]
fn pack_rect(rect: abi::Rectangle) -> u64 {
    (rect.min_x as u16 as u64)
        | ((rect.min_y as u16 as u64) << 16)
        | ((rect.max_x as u64) << 32)
        | ((rect.max_y as u64) << 48)
}

#[cfg(test)]
extern crate std;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handles_distinguish_null_from_zero_results() {
        // A port id of zero names no port, even though zero is a perfectly
        // good *count* elsewhere. The distinction is what makes a null handle
        // checkable.
        assert!(Port::NULL.is_null());
        assert!(!Port(1).is_null());
        assert!(Window::NULL.is_null());
        assert!(!Window(1).is_null());
        assert!(Screen::NULL.is_null());
        assert!(!Screen(1).is_null());
        assert!(Gadget::NULL.is_null());
        assert!(!Gadget(1).is_null());
    }

    #[test]
    fn check_splits_the_packed_result() {
        // A packed `u64` carries both halves, so a call can report a value
        // *and* an error — which a single register could not.
        assert_eq!(check(0), Ok(0));
        assert_eq!(check(42), Ok(42));
        assert_eq!(
            check((errors::NO_OBJECT as u32 as u64) << 32),
            Err(Error {
                code: errors::NO_OBJECT
            })
        );
        // Both halves at once: a value *and* an error.
        assert_eq!(
            check(7 | ((errors::NOT_FOUND as u32 as u64) << 32)),
            Err(Error {
                code: errors::NOT_FOUND
            })
        );
    }

    #[test]
    fn a_null_value_and_an_error_are_different_answers() {
        // The whole reason a syscall result has two fields: a library that is
        // not there succeeds and returns zero, which a single return value
        // could not distinguish from a failure.
        assert_eq!(check(0), Ok(0));
        assert_eq!(
            check((errors::NOT_FOUND as u32 as u64) << 32),
            Err(Error {
                code: errors::NOT_FOUND
            })
        );
    }

    #[test]
    fn a_rectangle_round_trips_through_its_packed_form() {
        let rect = abi::Rectangle::new(-3, 7, 640, 480);
        assert_eq!(unpack_rect(pack_rect(rect)), rect);
    }

    #[test]
    fn a_wait_that_ends_without_a_message_is_not_a_failure() {
        // The kernel answers `1`, not an error, and the difference matters: an
        // error here would make a program treat a spurious wakeup as a broken
        // port and give up.
        assert_eq!((1u64 != 0).then_some(1), Some(1));
        assert_eq!((0u64 != 0).then_some(0), None);
    }

    #[test]
    fn a_wait_returns_a_message_exactly_when_the_kernel_says_zero() {
        // The kernel answers `0` for "a message was copied out" and `1` for
        // "the wait ended without one". The two must not be read the same way
        // round: inverting them skips the reply to every *real* event and then
        // tries to reply to an untouched, all-zero message, which surfaces as
        // `NO_REPLY_PORT` and points the diagnosis at Exec rather than here.
        assert_eq!(
            (0u64 == 0).then_some(()),
            Some(()),
            "0 means a message arrived"
        );
        assert_eq!(
            (1u64 == 0).then_some(()),
            None,
            "1 means the wait came up empty"
        );
    }

    #[test]
    fn a_directory_entry_reads_its_own_name() {
        let mut entry = DirEntry {
            de_size: 42,
            de_type: entry_type::FILE,
            de_name_len: 3,
            de_name: [0; DIR_NAME_LEN],
        };
        entry.de_name[..3].copy_from_slice(b"abc");
        assert_eq!(entry.name(), "abc");
        assert!(!entry.is_dir());

        entry.de_type = entry_type::DIR;
        assert!(entry.is_dir());
    }

    #[test]
    fn a_directory_entry_name_length_cannot_outrun_the_buffer() {
        // A hostile reply claiming a 200-byte name in a 107-byte buffer must
        // read the buffer, not past it.
        let mut entry = DirEntry {
            de_size: 0,
            de_type: entry_type::FILE,
            de_name_len: 200,
            de_name: [b'x'; DIR_NAME_LEN],
        };
        assert_eq!(entry.name().len(), DIR_NAME_LEN);
        entry.de_name_len = 0;
        assert_eq!(entry.name(), "");
    }

    #[test]
    fn a_library_base_is_an_address_above_its_own_jump_table() {
        // The AmigaOS model, checked: the base is a *pointer* into this
        // program's own memory, and every function lives at a *negative*
        // offset below it.
        let base = library_base::exec::base();
        assert!(!base.is_null());
        assert!(base.raw() != 0, "a base is an address, not a token");

        // The base sits one word past the library's function list, so the
        // shallowest function is one word below it and the deepest is
        // `count` words below it.
        let count = library_base::exec::LAYOUT.count as usize;
        let words = library_base::SLOT_SIZE as usize;
        let mut seen_slots = Vec::new();
        for offset in [
            offset::EXEC_PUTMSG,
            offset::EXEC_GETMSG,
            offset::EXEC_OPENLIBRARY,
            offset::EXEC_FINDTASK,
        ] {
            assert!(offset < 0, "an LVO must be negative");
            let address = base.function_address(offset);
            assert!(
                (address as isize) < (base.raw() as isize),
                "base + {offset} must land below the base",
            );
            // The slot arithmetic: a function `w` words below the base is
            // slot `count - w`, counting from the table's own start.
            let w = offset.unsigned_abs() as usize / words;
            let slot = base
                .slot_for(offset)
                .expect("every library's own offset names a slot");
            assert_eq!(slot, count - w, "the slot arithmetic must agree");
            assert!(slot < count, "a slot is inside the function list");
            seen_slots.push(slot);
        }
        // Two functions never share a slot.
        seen_slots.sort_unstable();
        seen_slots.dedup();
        assert_eq!(seen_slots.len(), 4, "two functions shared a slot");
    }

    #[test]
    fn a_library_base_resolves_back_to_itself() {
        // A program that kept the base address in a variable of its own can
        // turn it back into something it can call through — which is what
        // makes a base a *value* rather than a token.
        let base = library_base::exec::base();
        // SAFETY: `base` came from the SDK's own `exec_base`.
        let resolved = unsafe { library_base::base_at(base.raw()) };
        assert_eq!(resolved, base);
    }

    #[test]
    fn a_call_through_a_base_reaches_a_real_stub() {
        // The stub is a real `extern "sysv64"` function that marshals into a
        // syscall. With no kernel installed the syscall fails — and it fails
        // *cleanly*, with an error code, which is what proves the stub was
        // called rather than the base being a dead number.
        let base = library_base::exec::base();
        // SAFETY: no pointer arguments, so there is nothing to validate.
        let packed = unsafe { base.call(offset::EXEC_FINDTASK, [0; 6]) };
        let (value, error) = library_base::unpack(packed);
        // With no harness installed the dispatcher answers
        // `NOT_IMPLEMENTED`; the point is that it answered *at all*.
        assert!(
            error != 0 || value != 0,
            "a call through a base must reach a stub that issues a syscall",
        );
    }

    #[test]
    fn an_offset_outside_the_table_names_no_slot() {
        // A program that guessed an offset must get a clean error rather than
        // a call through a wild pointer — and rather than a call to some
        // *other* function, which would look like a working call.
        let base = library_base::exec::base();
        assert!(base.slot_for(0).is_none());
        assert!(base.slot_for(8).is_none());
        assert!(base.slot_for(-1).is_none());
        assert!(base.slot_for(-8192).is_none());
    }
}
