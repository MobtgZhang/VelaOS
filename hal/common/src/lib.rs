//! Architecture-independent HAL traits.
//!
//! These traits define the surface every VelaOS architecture must provide.
//! Nothing here contains architecture code: the kernel talks to hardware only
//! through these abstractions, which is what keeps `kernel/*` free of paging,
//! port-I/O and interrupt-controller details.
//!
//! Implementations live in `hal/x86_64` (v0.1.0) and, in later milestones,
//! `hal/aarch64`, `hal/riscv64` and `hal/loongarch64`.

#![no_std]

use core::fmt;

/// Page size used across all VelaOS architectures.
pub const PAGE_SIZE: u64 = 4096;

/// A physical address.
pub type PhysAddr = u64;
/// A virtual address.
pub type VirtAddr = u64;

/// Errors a virtual-memory implementation may report.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MapError {
    /// The virtual range is not page aligned.
    MisalignedVirtual,
    /// The physical range is not page aligned.
    MisalignedPhysical,
    /// `va + len` or `pa + len` wrapped the address space.
    AddressOverflow,
    /// The address is already mapped.
    AlreadyMapped,
    /// The address is not mapped.
    NotMapped,
    /// The mapping would exceed the architecture's address space or the
    /// available physical memory.
    OutOfMemory,
}

impl fmt::Display for MapError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Self::MisalignedVirtual => "virtual address is not page aligned",
            Self::MisalignedPhysical => "physical address is not page aligned",
            Self::AddressOverflow => "address range overflowed",
            Self::AlreadyMapped => "address is already mapped",
            Self::NotMapped => "address is not mapped",
            Self::OutOfMemory => "out of memory",
        };
        f.write_str(s)
    }
}

/// Access permissions and page attributes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PageFlags {
    /// Page is readable.
    pub readable: bool,
    /// Page is writable.
    pub writable: bool,
    /// Page is executable.
    pub executable: bool,
    /// Page is accessible from ring 3.
    pub user: bool,
    /// Page is writable from ring 3 (implies `user`).
    pub user_writable: bool,
    /// Page must not be cached, for MMIO.
    pub uncacheable: bool,
}

impl Default for PageFlags {
    /// Kernel read/write, non-executable, supervisor-only, cacheable.
    fn default() -> Self {
        Self {
            readable: true,
            writable: true,
            executable: false,
            user: false,
            user_writable: false,
            uncacheable: false,
        }
    }
}

impl PageFlags {
    /// Fully readable, writable, non-executable kernel page.
    pub const fn kernel_rw() -> Self {
        Self {
            readable: true,
            writable: true,
            executable: false,
            user: false,
            user_writable: false,
            uncacheable: false,
        }
    }

    /// Kernel readable, writable, executable — for kernel text.
    pub const fn kernel_rx() -> Self {
        Self {
            readable: true,
            writable: false,
            executable: true,
            user: false,
            user_writable: false,
            uncacheable: false,
        }
    }

    /// Read-only kernel page — this is the W^X partner to [`Self::kernel_rw`].
    pub const fn kernel_ro() -> Self {
        Self {
            readable: true,
            writable: false,
            executable: false,
            user: false,
            user_writable: false,
            uncacheable: false,
        }
    }

    /// Read/write user page.
    pub const fn user_rw() -> Self {
        Self {
            readable: true,
            writable: true,
            executable: false,
            user: true,
            user_writable: true,
            uncacheable: false,
        }
    }

    /// Read-only user page — this is where user code lives under W^X.
    pub const fn user_rx() -> Self {
        Self {
            readable: true,
            writable: false,
            executable: true,
            user: true,
            user_writable: false,
            uncacheable: false,
        }
    }

    /// Read/write supervisor page, no caching — for MMIO and the framebuffer.
    pub const fn mmio() -> Self {
        Self {
            readable: true,
            writable: true,
            executable: false,
            user: false,
            user_writable: false,
            uncacheable: true,
        }
    }

    /// True when this mapping is writable and executable at the same time,
    /// which VelaOS treats as a policy violation.
    pub const fn is_wx_violation(&self) -> bool {
        self.writable && self.executable
    }
}

/// An opaque address space handle.
///
/// A design decision rather than a raw CR3: on x86-64 an address space is a
/// PML4, but on LoongArch it is a different structure entirely, and the kernel
/// must not be able to tell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AddressSpaceId(pub u64);

/// CPU control operations every architecture provides.
pub trait CpuArch {
    /// Halt the current CPU until the next interrupt.
    fn halt();
    /// Enable interrupts at the CPU level.
    fn enable_interrupts();
    /// Disable interrupts at the CPU level.
    fn disable_interrupts();
    /// True when interrupts are enabled.
    fn interrupts_enabled() -> bool;
    /// Identifier of the current logical CPU.
    fn current_cpu_id() -> usize;
    /// Spin until the previous `busy_wait_cycles` call returns, as a spinlock
    /// fallback on single-core configurations.
    fn busy_wait_cycles() -> u64;
}

/// Page-table management.
pub trait VirtualMemory {
    /// Create a fresh, empty address space.
    fn new_address_space() -> Result<AddressSpaceId, MapError>;

    /// Map `len` bytes of `pa` at `va` in `space`.
    ///
    /// `len` must be a multiple of [`PAGE_SIZE`], and both `va` and `pa` must
    /// be page aligned.
    fn map(
        space: AddressSpaceId,
        va: VirtAddr,
        pa: PhysAddr,
        len: usize,
        flags: PageFlags,
    ) -> Result<(), MapError>;

    /// Remove the mapping containing `va`.
    fn unmap(space: AddressSpaceId, va: VirtAddr) -> Result<(), MapError>;

    /// Translate `va`, returning the physical address if mapped.
    fn translate(space: AddressSpaceId, va: VirtAddr) -> Option<PhysAddr>;

    /// Map `len` bytes of `pa` at `va` in the **current** address space.
    ///
    /// Used during boot before any task exists and therefore before any
    /// address space is switched.
    fn map_current(
        va: VirtAddr,
        pa: PhysAddr,
        len: usize,
        flags: PageFlags,
    ) -> Result<(), MapError>;

    /// Unmap in the current address space.
    fn unmap_current(va: VirtAddr) -> Result<(), MapError>;

    /// Switch the CPU to `space`.
    fn activate(space: AddressSpaceId);
}

/// Monotonic timekeeping.
///
/// Ticks are opaque; only differences are meaningful.
pub trait PlatformTimer {
    /// Ticks since the timer was started.
    fn monotonic_ticks() -> u64;

    /// Frequency of [`Self::monotonic_ticks`] in Hz.
    fn ticks_per_second() -> u64;

    /// Blocks the current CPU for at least `ticks`.
    fn busy_wait(ticks: u64);
}

/// Non-arch-specific console output.
///
/// Present so the kernel logger has a single sink across architectures; the
/// x86-64 implementation is a COM1 serial driver.
pub trait Console {
    /// Write a single byte.
    fn put_byte(byte: u8);
    /// Write a string.
    fn put_str(s: &str);
}

#[cfg(test)]
extern crate std;

#[cfg(test)]
mod tests {
    use super::*;

    use std::vec::Vec;

    #[test]
    fn page_flags_defaults_are_kernel_rw() {
        let f = PageFlags::default();
        assert!(f.readable && f.writable);
        assert!(!f.executable);
        assert!(!f.user);
        assert!(!f.is_wx_violation());
    }

    #[test]
    fn wx_combination_is_flagged() {
        let f = PageFlags {
            writable: true,
            executable: true,
            ..PageFlags::kernel_rw()
        };
        assert!(f.is_wx_violation());
    }

    #[test]
    fn user_pages_separate_code_from_data() {
        // User code must be executable but not writable; user data the reverse.
        assert!(PageFlags::user_rx().executable && !PageFlags::user_rx().writable);
        assert!(PageFlags::user_rw().writable && !PageFlags::user_rw().executable);
        assert!(!PageFlags::user_rx().is_wx_violation());
        assert!(!PageFlags::user_rw().is_wx_violation());
    }

    #[test]
    fn mmio_is_uncacheable_and_supervisor_only() {
        let f = PageFlags::mmio();
        assert!(f.uncacheable);
        assert!(!f.user);
    }

    #[test]
    fn map_error_messages_are_distinct() {
        let all = [
            MapError::MisalignedVirtual,
            MapError::MisalignedPhysical,
            MapError::AddressOverflow,
            MapError::AlreadyMapped,
            MapError::NotMapped,
            MapError::OutOfMemory,
        ];
        // Render each variant to text and assert the strings are unique, so a
        // future variant cannot silently shadow an existing message.
        let msgs: Vec<std::string::String> = all
            .iter()
            .map(|e| {
                use core::fmt::Write as _;
                let mut s = std::string::String::new();
                let _ = write!(s, "{e}");
                s
            })
            .collect();
        let mut sorted = msgs.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(msgs.len(), sorted.len(), "duplicate error message");
    }
}
