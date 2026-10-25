//! `vela-sys` — the VelaOS user-space SDK.
//!
//! # What this crate is
//!
//! The ring 3 half of VelaOS: the library calls an AmigaOS program makes, and
//! the syscall gate behind them. A user program links this, opens
//! `intuition.library`, calls `OpenWindow`, and gets back a window id — with
//! no knowledge that any of it crossed an address space.
//!
//! # The three rules it exists to enforce
//!
//! 1. **The kernel never hands out a pointer.** `OpenLibrary` returns a
//!    [`LibraryHandle`], `OpenWindow` a window id, `AddPort` a port id. Every
//!    one is an opaque integer the kernel can revoke. A pointer into another
//!    task's memory would be the opposite of all of that.
//! 2. **Every user pointer is validated.** The kernel copies *from* and *to*
//!    user memory through probing helpers, so a program that passes a bad
//!    address gets an error rather than a fault — and the conformance suite
//!    asserts on it.
//! 3. **The library base is a pointer, but a harmless one.** See
//!    [`library_base`]: it points into the program's *own* address space at
//!    its own jump table. That is what makes `A6`-relative calls with
//!    negative offsets work, and it is why a program that scribbles on it
//!    corrupts only itself.
//!
//! # The shape of a program
//!
//! Two layers, and a program uses the upper one. [`library_base`] holds the
//! AmigaOS calling model — a base, negative offsets, a jump table — and [`api`]
//! puts ordinary Rust calls on top of it. A call through [`api`] is one
//! jump-table slot, which is one `syscall`; the lower layer exists so that the
//! `A6` convention means something real, not so that programs have to use it by
//! hand.
//!
//! ```no_run
//! use vela_sys::api::{self, idcmp, request_codes, window_flags};
//! use vela_sys::abi::{OpenWindowArgs, Rectangle};
//!
//! // A port to receive events on. `exec` is always open, so nothing needs
//! // opening before this.
//! let port = api::create_port(42).unwrap();
//!
//! // Everything else is opened by name, as on AmigaOS.
//! api::open_library("intuition.library").unwrap();
//!
//! // A window lives on a screen, so the screen comes first.
//! let screen = api::open_screen("Workbench", &Rectangle::new(0, 0, 1280, 800)).unwrap();
//!
//! let args = OpenWindowArgs {
//!     x: 100,
//!     y: 100,
//!     width: 320,
//!     height: 200,
//!     idcmp: idcmp::ALL,
//!     flags: window_flags::CLOSEGADGET,
//! };
//! let window = api::open_window(screen, "Demo", &args, port).unwrap();
//!
//! // Events arrive as Exec messages on the program's own port — not on a
//! // private side channel — and are replied to like any other message.
//! while let Some(mut msg) = api::wait_port(port).unwrap() {
//!     let mut reply = vela_sys::abi::Message::empty();
//!     let event = msg.as_intui();
//!     api::reply_msg(&mut msg, &mut reply).unwrap();
//!     if event.im_Type == request_codes::CLOSE {
//!         break;
//!     }
//! }
//!
//! api::close_window(window).unwrap();
//! api::close_screen(screen).unwrap();
//! ```
//!
//! # Module map
//!
//! | Module | Responsibility |
//! |---|---|
//! | [`api`] | The typed calls a program actually makes |
//! | [`abi`] | The AmigaOS layouts a program passes and receives |
//! | [`numbers`] | The syscall numbers and error codes, mirrored from the kernel |
//! | [`syscall`] | The `syscall` instruction and its result |
//! | [`library_base`] | `A6`-relative calls through a per-process jump table |
//! | [`hosted`] | The direct-call path that makes all of this testable under `cargo test` |
//!
//! The prelude is [`prelude`] — everything a program normally wants, in one
//! `use`.

#![no_std]

extern crate alloc;

pub mod abi;
pub mod api;
pub mod heap;
pub mod library_base;
pub mod numbers;
pub mod runtime;
pub mod syscall;

#[cfg(not(target_os = "none"))]
pub mod hosted;

pub use api::power_off;
pub use api::{Gadget, Port, Screen, Window};
pub use library_base::{LibraryBase, Slot};
pub use syscall::SyscallResult;

/// A failed call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Error {
    /// The syscall layer's error code.
    ///
    /// One of [`numbers::errors`]. A distinct type rather than an `i32` so a
    /// program cannot mistake an error code for a value.
    pub code: i32,
}

impl Error {
    /// True when this is "the library is not implemented" — distinct from
    /// "the call failed", and worth telling apart in a report.
    pub const fn is_not_implemented(self) -> bool {
        self.code == numbers::errors::NOT_IMPLEMENTED
    }

    /// True when a user pointer was not valid.
    ///
    /// VelaOS's own error: on AmigaOS this was a crash.
    pub const fn is_bad_pointer(self) -> bool {
        self.code == numbers::errors::BAD_POINTER
    }
}

/// The result of a library call.
pub type Result<T> = core::result::Result<T, Error>;

/// The error codes the syscall layer returns.
///
/// Re-exported at the crate root so a caller can match on one without reaching
/// into [`api`] or [`numbers`]. `api::errors` is the same set — this is the
/// shorter spelling, not a second list.
pub use crate::numbers::errors;

/// Everything a program normally wants.
pub mod prelude {
    pub use crate::abi::{
        buttons, gadget_kind, idcmp, mouse_codes, node_type, request_codes, sigs, window_flags,
        Bptr, IntuiMessage, LibraryHandle, LibraryId, Message, Node, OpenWindowArgs, Rectangle,
    };
    pub use crate::api;
    pub use crate::library_base::{exec_base, LibraryBase};
    pub use crate::numbers::errors;
    pub use crate::{Error, Result};
}

#[cfg(test)]
mod tests {
    use super::prelude::*;
    use crate::library_base::{offset, pack, unpack};

    #[test]
    fn the_syscall_boundary_splits_value_from_error() {
        // The split is the whole reason `SyscallResult` has two fields: a
        // single return value cannot say "succeeded, and the answer was
        // NULL", which `OpenLibrary` for a missing library has to.
        let result = crate::SyscallResult {
            value: 0,
            error: errors::NO_OBJECT,
        };
        assert!(result.is_err());
        assert_eq!(result.value_or(7), 7, "a failed call yields the fallback");
    }

    #[test]
    fn exec_is_the_only_library_that_needs_no_opening() {
        assert!(!exec_base().is_null());
        assert_eq!(exec_base().name(), "exec.library");
    }

    #[test]
    fn a_library_base_addresses_a_function_by_negative_offset() {
        let base = exec_base();
        let address = base.function_address(offset::EXEC_PUTMSG);
        assert!(
            (address as isize) < (base.raw() as isize),
            "a negative offset must address below the base",
        );
    }

    #[test]
    fn a_packed_stub_result_carries_both_halves() {
        // The jump table's return convention is the boundary's, narrowed to
        // one register.
        let packed = pack(0x1234, errors::NO_MESSAGE);
        let (value, error) = unpack(packed);
        assert_eq!(value, 0x1234);
        assert_eq!(error, errors::NO_MESSAGE);
    }
}
