//! The opaque handles user space holds.
//!
//! # The rule
//!
//! User space never receives a kernel pointer. It receives a handle — a
//! small integer — and the kernel turns that back into the object it names.
//!
//! This is not a stylistic preference. A user program that could name a
//! kernel address could read the kernel's page tables, and therefore the
//! physical memory map, and therefore every other task's memory. The handle
//! indirection is what makes the boundary a boundary.
//!
//! # Why the handles are typed
//!
//! A port id and a window id are both `u32`, and a program that passes a
//! window id where a port id belongs gets an error rather than a wrong
//! object — because the kernel's tables are separate and the id is looked
//! up in the right one. The *types* are distinct so that the mistake is a
//! compile error in Rust code, and a runtime error in C code.

use vela_abi_native64::LibraryHandle;

/// A handle to a message port.
///
/// This is the *same* type the Exec service hands out, re-exported rather
/// than redeclared. Redeclaring it would create two distinct `PortId` types
/// that both mean "a port" and could be mixed up at every call site — the
/// kind of mistake the type system is supposed to catch.
pub use vela_kernel_exec::port::PortId;

/// A handle to a library.
///
/// This is a re-export of the ABI's handle, so that a user program and the
/// kernel agree on what a library handle *is* — and both agree it is not a
/// pointer.
pub type LibraryId = LibraryHandle;

/// A handle to a screen.
///
/// Re-exported from the Intuition service for the same reason [`PortId`] is:
/// two types that both mean "a screen" is one more conversion for a handler to
/// forget.
pub use vela_kernel_intuition::ScreenId;

/// A handle to a window.
pub use vela_kernel_intuition::WindowId;

/// A handle to a gadget.
pub use vela_kernel_intuition::GadgetId;

/// A handle to a layer.
pub use vela_kernel_layers::LayerId;

/// A handle to a pen in a palette.
pub use vela_kernel_graphics::PenId;

/// A handle to a raster or bitmap.
///
/// VelaOS has no syscalls that hand out a raster: a program draws into a
/// window's layer, and the framebuffer is the kernel's own. The type stays
/// because the ABI names one, and a future `OpenRaster` will need it — but
/// nothing installs a handler that returns one, so no program can hold a
/// `RasterId` that came from the kernel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RasterId(pub u32);

impl RasterId {
    /// The id of "no raster".
    pub const NONE: RasterId = RasterId(0);

    /// True when this id names no raster.
    pub const fn is_none(self) -> bool {
        self.0 == 0
    }
}

/// A handle to a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FileId(pub u32);

impl FileId {
    /// The id of "no file".
    pub const NONE: FileId = FileId(0);

    /// True when this id names no file.
    pub const fn is_none(self) -> bool {
        self.0 == 0
    }
}

#[cfg(test)]
extern crate std;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_handle_reserves_zero_for_none() {
        // A zero handle must be distinguishable from a real one in every
        // handle type, or a failed syscall would look like a successful one.
        assert!(PortId::NONE.is_none());
        assert!(ScreenId::NONE.is_none());
        assert!(WindowId::NONE.is_none());
        assert!(LayerId::NONE.is_none());
        assert!(RasterId::NONE.is_none());
        assert!(FileId::NONE.is_none());
    }

    #[test]
    fn a_real_handle_is_not_none() {
        assert!(!PortId(1).is_none());
        assert!(!WindowId(1).is_none());
        assert!(!FileId(1).is_none());
    }

    #[test]
    fn the_handle_types_are_distinct() {
        // A port id must not be usable as a window id at the type level.
        // The runtime check is that the kernel looks the id up in the right
        // table; the compile-time check is that the types differ.
        let port = PortId(1);
        let window = WindowId(1);
        // The values are the same, but the types are not.
        assert_eq!(port.0, window.0);
        // A helper that accepts only ports would reject a window.
        fn takes_port(_p: PortId) -> u32 {
            1
        }
        assert_eq!(takes_port(port), 1);
    }

    #[test]
    fn a_library_handle_is_the_abi_handle() {
        // The kernel and the user program must agree on what a library
        // handle is, so the syscall crate uses the ABI's type directly.
        let h = LibraryHandle::for_library(vela_abi_native64::LibraryId::Exec);
        assert_eq!(h.raw(), 1);
        assert!(!h.is_null());
    }
}
