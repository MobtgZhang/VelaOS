//! Identity-mapped DMA allocations.
//!
//! VelaOS's kernel heap is identity mapped. Consequently an allocation's
//! virtual address can be programmed into xHCI as its DMA address directly.
//! These allocations deliberately have no `Drop`: controller structures and
//! transfer buffers must remain valid for the lifetime of the kernel.

use alloc::vec;
use alloc::vec::Vec;
use core::alloc::Layout;
use core::ptr;

/// A zeroed, aligned allocation permanently owned by the driver.
pub(crate) struct DmaBuffer {
    ptr: *mut u8,
    len: usize,
}

// SAFETY: the allocation is uniquely owned, never freed, and all access from
// the timer tick is serialised by the controller mutex. xHCI may access the
// backing memory independently, which is why accessors use volatile I/O.
unsafe impl Send for DmaBuffer {}

impl DmaBuffer {
    /// Allocate `len` zeroed bytes at an xHCI-compatible alignment.
    pub(crate) fn new(len: usize, align: usize) -> Result<Self, i32> {
        if len == 0 || align == 0 || !align.is_power_of_two() {
            return Err(-12);
        }
        let layout = Layout::from_size_align(len, align).map_err(|_| -12)?;
        // SAFETY: `layout` has a non-zero size and valid power-of-two alignment.
        // The pointer is checked rather than passing it to handle_alloc_error.
        let ptr = unsafe { alloc::alloc::alloc_zeroed(layout) };
        if ptr.is_null() {
            return Err(-12);
        }
        Ok(Self { ptr, len })
    }

    pub(crate) fn addr(&self) -> u64 {
        self.ptr as u64
    }

    pub(crate) fn len(&self) -> usize {
        self.len
    }

    pub(crate) fn as_mut_ptr(&self) -> *mut u8 {
        self.ptr
    }

    /// Overwrite the allocation without letting LLVM elide DMA-visible stores.
    pub(crate) fn zero(&self) {
        for index in 0..self.len {
            // SAFETY: `index` is within this live allocation.
            unsafe { ptr::write_volatile(self.ptr.add(index), 0) };
        }
    }

    pub(crate) fn read_u32(&self, offset: usize) -> u32 {
        if offset + core::mem::size_of::<u32>() > self.len {
            return 0;
        }
        // SAFETY: the bounds check covers the complete naturally sized load.
        unsafe { ptr::read_volatile(self.ptr.add(offset).cast::<u32>()) }
    }

    pub(crate) fn write_u32(&self, offset: usize, value: u32) {
        if offset + core::mem::size_of::<u32>() <= self.len {
            // SAFETY: the bounds check covers the complete naturally sized store.
            unsafe { ptr::write_volatile(self.ptr.add(offset).cast::<u32>(), value) };
        }
    }

    pub(crate) fn write_u64(&self, offset: usize, value: u64) {
        if offset + core::mem::size_of::<u64>() <= self.len {
            // SAFETY: the bounds check covers the complete naturally sized store.
            unsafe { ptr::write_volatile(self.ptr.add(offset).cast::<u64>(), value) };
        }
    }

    /// Copy bytes from ordinary memory into a DMA buffer.
    pub(crate) fn copy_from(&self, source: &[u8]) -> Result<(), i32> {
        if source.len() > self.len {
            return Err(-22);
        }
        for (index, byte) in source.iter().enumerate() {
            // SAFETY: the length check above covers the complete source range.
            unsafe { ptr::write_volatile(self.ptr.add(index), *byte) };
        }
        Ok(())
    }

    /// Read bytes written by the controller into an owned descriptor buffer.
    pub(crate) fn to_vec(&self, len: usize) -> Vec<u8> {
        let len = len.min(self.len);
        let mut bytes = vec![0; len];
        for (index, byte) in bytes.iter_mut().enumerate() {
            // SAFETY: `len` was clamped to the allocation size.
            *byte = unsafe { ptr::read_volatile(self.ptr.add(index)) };
        }
        bytes
    }

    /// Read at most eight report bytes without allocating in interrupt context.
    pub(crate) fn read_report(&self, len: usize) -> ([u8; 8], usize) {
        let len = len.min(self.len).min(8);
        let mut report = [0; 8];
        for (index, byte) in report.iter_mut().take(len).enumerate() {
            // SAFETY: `len` was clamped to the allocation size.
            *byte = unsafe { ptr::read_volatile(self.ptr.add(index)) };
        }
        (report, len)
    }
}
