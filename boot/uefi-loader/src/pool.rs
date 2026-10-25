//! A fixed page pool taken before `ExitBootServices`.
//!
//! Everything the kernel needs must already exist once firmware hands over:
//! after `ExitBootServices` the `uefi` crate's allocator is dead, and calling
//! into it panics. So the loader takes **one** contiguous reservation up front
//! and sub-allocates from it. That single reservation is also what guarantees
//! the kernel image, its stacks and `BootInfo` are never recycled as free
//! memory, because the pool pages stay marked as loader data in the memory map
//! handed to the kernel.

use core::ptr::NonNull;

use uefi::boot::AllocateType;
use uefi::mem::memory_map::MemoryType;
use uefi::Status;

/// A page-aligned bump allocator over one contiguous UEFI reservation.
pub struct Pool {
    /// Physical base of the reservation.
    base: u64,
    /// Total bytes reserved.
    len: u64,
    /// Bytes already handed out.
    used: u64,
}

impl Pool {
    /// Reserve `pages` contiguous pages of loader data.
    pub fn new(pages: u64) -> Result<Self, Status> {
        let bytes = pages.checked_mul(4096).ok_or(Status::OUT_OF_RESOURCES)?;
        // SAFETY: the returned pointer identifies a valid allocation. We only
        // record its address and length; we never create a reference to it,
        // because the firmware has not initialised the contents.
        let ptr: NonNull<u8> = match uefi::boot::allocate_pages(
            AllocateType::AnyPages,
            MemoryType::LOADER_DATA,
            pages as usize,
        ) {
            Ok(p) => p,
            Err(e) => return Err(e.status()),
        };
        Ok(Self {
            base: ptr.as_ptr() as u64,
            len: bytes,
            used: 0,
        })
    }

    /// Total pages reserved.
    pub const fn pages(&self) -> u64 {
        self.len / 4096
    }

    /// The physical base of the reservation.
    pub const fn base(&self) -> u64 {
        self.base
    }

    /// The total byte length of the reservation.
    pub const fn len(&self) -> u64 {
        self.len
    }

    /// Bytes still available.
    pub const fn remaining(&self) -> u64 {
        self.len - self.used
    }

    /// Hand out `bytes` starting at an `align`-aligned offset.
    ///
    /// # Errors
    ///
    /// `OUT_OF_RESOURCES` when the request does not fit. The pool never grows:
    /// a request that does not fit is a build-time sizing bug, and failing
    /// loudly at boot is far easier to diagnose than a corrupted handoff.
    pub fn alloc_pages(&mut self, bytes: u64, align: u64) -> Result<u64, Status> {
        let align = align.max(1);
        // Round the current offset up to `align`, computing the padding with a
        // subtraction so a hostile `align` cannot wrap.
        let remainder = self.used % align;
        let padding = if remainder == 0 { 0 } else { align - remainder };
        let start = self
            .used
            .checked_add(padding)
            .ok_or(Status::OUT_OF_RESOURCES)?;
        let end = start.checked_add(bytes).ok_or(Status::OUT_OF_RESOURCES)?;
        if end > self.len {
            return Err(Status::OUT_OF_RESOURCES);
        }
        self.used = end;
        Ok(self.base + start)
    }
}
