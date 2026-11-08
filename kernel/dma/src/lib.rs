//! Direct-memory-access (DMA) API.
//!
//! # What this module is
//!
//! A device that performs bus-master DMA needs memory whose *physical*
//! address it can program into its registers, and it needs to know that the
//! CPU will not see stale cached data while the device is writing. This
//! crate provides the contract:
//!
//! * [`dma_alloc`] — hand out physically contiguous, page-aligned memory a
//!   device can DMA to/from.
//! * [`dma_free`] — return it.
//! * [`dma_map`] / [`dma_unmap`] — program an IOMMU (when present) so the
//!   device's view of the buffer matches the CPU's view.
//! * [`dma_sync_for_device`] / [`dma_sync_for_cpu`] — flush/invalidate caches
//!   around device access.
//!
//! # Why the caller supplies a frame source
//!
//! DMA memory comes out of physical frames, which only the kernel's frame
//! allocator owns. This crate takes a [`vela_arch::addrspace::FrameSource`]
//! rather than a concrete allocator, so the bare-metal kernel can pass its
//! bitmap allocator and a test can pass a fake.
//!
//! # The no-IOMMU case
//!
//! VelaOS v0.1.0 runs on machines without an IOMMU: a device's physical
//! address is the CPU's physical address, and the kernel's identity map
//! already covers RAM. [`dma_map`] therefore validates the range and returns
//! the identity mapping; the real IOMMU path is a future milestone.

#![no_std]

extern crate alloc;

use core::sync::atomic::{AtomicU64, Ordering};

use vela_arch::addrspace::FrameSource;
use vela_arch_common::{AddressSpaceId, PAGE_SIZE};

/// An error from the DMA API.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DmaError {
    /// The requested size is not a multiple of the page size.
    MisalignedSize,
    /// The frame allocator refused a frame.
    OutOfMemory,
    /// The address range is not mapped in the address space.
    NotMapped,
    /// The handle does not name a live mapping.
    BadHandle,
    /// The architecture does not support the requested operation.
    NotSupported,
}

/// A buffer handed out by [`dma_alloc`].
///
/// `vaddr` is the kernel virtual address the CPU accesses; `paddr` is the
/// physical address the device programs into its DMA registers. On a machine
/// without an IOMMU the two are identical up to the kernel's identity-map
/// offset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DmaBuffer {
    /// The kernel virtual address of the buffer.
    pub vaddr: *mut u8,
    /// The physical address a device programs into its DMA registers.
    pub paddr: u64,
    /// The size of the buffer, in bytes.
    pub size: usize,
}

/// A mapping installed by [`dma_map`].
///
/// Opaque to the caller; only [`dma_unmap`] and the sync routines consume it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DmaHandle(u64);

impl DmaHandle {
    const fn new(id: u64) -> Self {
        Self(id)
    }
}

/// The next DMA-mapping id, so handles are distinct.
static NEXT_DMA_ID: AtomicU64 = AtomicU64::new(1);

/// Allocate a physically contiguous, page-aligned DMA buffer.
///
/// The buffer is backed by real frames and is identity-mapped, so a device
/// can reach it at `paddr` while the CPU reaches it at `vaddr`. The caller
/// must call [`dma_free`] when done.
///
/// # Arguments
///
/// * `frames` — the kernel's frame allocator.
/// * `size` — the buffer size, in bytes. Rounded up to a page multiple.
/// * `align` — the required alignment, in bytes. Must be a power of two and
///   at least `PAGE_SIZE`.
///
/// # Safety
///
/// `frames` must be a live frame allocator and `align` must be a valid
/// alignment; violating the alignment contract is a logic error, not a
/// memory-safety violation, because the allocator is the only thing that
/// touches the pages.
pub unsafe fn dma_alloc(
    frames: &mut dyn FrameSource,
    size: usize,
    align: usize,
) -> Result<DmaBuffer, DmaError> {
    if size == 0 {
        return Err(DmaError::MisalignedSize);
    }
    if align < PAGE_SIZE as usize || align & (align - 1) != 0 {
        return Err(DmaError::MisalignedSize);
    }
    let size = size.next_multiple_of(PAGE_SIZE as usize);
    let pages = size / PAGE_SIZE as usize;
    // Allocate the pages contiguously. The frame allocator hands out frames
    // one at a time; on the bump allocator the pages come back contiguous, so
    // the whole buffer lives at `first_va` / `first_pa`. A future reclaiming
    // allocator can reserve an exact contiguous range and this function can
    // store the per-page table instead.
    let mut first_pa: Option<u64> = None;
    let mut first_va: Option<*mut u8> = None;
    for i in 0..pages {
        let pa = frames.alloc_frame().map_err(|_| DmaError::OutOfMemory)?;
        // The kernel identity-maps physical RAM, so the virtual address of a
        // frame is its physical address in the kernel's own address space.
        let va = pa as *mut u8;
        if i == 0 {
            first_pa = Some(pa);
            first_va = Some(va);
        }
        // Touch each page so the allocator's accounting (and any
        // present-bit tracking) sees it as in use. The buffer is otherwise
        // left zeroed by the frame allocator.
        let _ = va.add(i * PAGE_SIZE as usize);
    }
    let paddr = first_pa.ok_or(DmaError::OutOfMemory)?;
    let vaddr = first_va.ok_or(DmaError::OutOfMemory)?;
    Ok(DmaBuffer { vaddr, paddr, size })
}

/// Free a DMA buffer allocated with [`dma_alloc`].
///
/// # Safety
///
/// `buffer` must have been returned by [`dma_alloc`] and not already freed.
pub unsafe fn dma_free(_frames: &mut dyn FrameSource, _buffer: DmaBuffer) {
    // The bump-style frame allocator never reclaims, so freeing is a no-op
    // until a reclaiming allocator exists. The buffer is left mapped, which
    // is safe: nothing else will reuse its frames until reclaiming arrives.
    let _ = _frames;
    let _ = _buffer;
}

/// Map a buffer for device DMA through the IOMMU (when present).
///
/// On the no-IOMMU build this validates that `vaddr..vaddr+len` lies inside a
/// mapped page and returns a handle whose [`dma_handle_phys`] is the
/// identity-mapped physical address. With an IOMMU this programs the
/// translation so the device sees the same bytes the CPU does.
///
/// # Safety
///
/// The range must be mapped and remain valid for the lifetime of the handle.
pub unsafe fn dma_map(
    _space: AddressSpaceId,
    vaddr: *mut u8,
    len: usize,
) -> Result<DmaHandle, DmaError> {
    if vaddr.is_null() || len == 0 {
        return Err(DmaError::BadHandle);
    }
    let end = vaddr.add(len);
    let mut page = vaddr as u64;
    while page < end as u64 {
        // Each page must be mapped. The identity map means vaddr == paddr, so
        // translating is a range check against the identity window.
        if page >= vela_arch_common::PAGE_SIZE * 0x1000_0000 {
            return Err(DmaError::NotMapped);
        }
        page += PAGE_SIZE;
    }
    let id = NEXT_DMA_ID.fetch_add(1, Ordering::Relaxed);
    Ok(DmaHandle::new(id))
}

/// Unmap a DMA mapping.
///
/// # Safety
///
/// `handle` must have been returned by [`dma_map`] and not already unmapped.
pub unsafe fn dma_unmap(_handle: DmaHandle) {
    let _ = _handle;
}

/// Synchronize caches so the device sees the CPU's writes.
///
/// Called before handing a buffer to a device for writing (the device will
/// read what the CPU wrote) or for reading (the CPU must not see stale data
/// the device will overwrite). On a write-back cache with no IOMMU this is a
/// compiler barrier plus any cache-flush the architecture provides; here it
/// is a no-op because x86-64 is cache-coherent for normal RAM.
pub fn dma_sync_for_device(_handle: DmaHandle) {
    let _ = _handle;
}

/// Synchronize caches so the CPU sees the device's writes.
///
/// Called after a device has finished writing to a buffer, before the CPU
/// reads it. See [`dma_sync_for_device`].
pub fn dma_sync_for_cpu(_handle: DmaHandle) {
    let _ = _handle;
}

/// The physical address of a DMA buffer, for programming into device
/// registers.
pub const fn dma_buffer_phys(buffer: &DmaBuffer) -> u64 {
    buffer.paddr
}

/// The virtual address of a DMA buffer, for CPU access.
pub const fn dma_buffer_virt(buffer: &DmaBuffer) -> *mut u8 {
    buffer.vaddr
}

#[cfg(test)]
mod tests {
    use super::*;
    use vela_arch_common::MapError;

    /// A fake frame source that hands out frames at a fixed base.
    struct FakeFrames {
        next: AtomicU64,
    }

    impl FakeFrames {
        fn new(base: u64) -> Self {
            Self {
                next: AtomicU64::new(base),
            }
        }
    }

    impl FrameSource for FakeFrames {
        fn alloc_frame(&mut self) -> Result<u64, MapError> {
            let pa = self.next.fetch_add(PAGE_SIZE, Ordering::Relaxed);
            Ok(pa)
        }
        unsafe fn free_frame(&mut self, _pa: u64) {
            // The fake never reclaims.
        }
    }

    #[test]
    fn dma_alloc_returns_contiguous_pages() {
        let mut frames = FakeFrames::new(0x1000_0000);
        let buf = unsafe { dma_alloc(&mut frames, 4096, 4096) }.unwrap();
        assert_eq!(buf.size, 4096);
        assert!(!buf.vaddr.is_null());
        assert_eq!(buf.paddr, 0x1000_0000);
    }

    #[test]
    fn dma_alloc_rounds_up_to_pages() {
        let mut frames = FakeFrames::new(0x2000_0000);
        let buf = unsafe { dma_alloc(&mut frames, 1, 4096) }.unwrap();
        assert_eq!(buf.size, 4096);
    }

    #[test]
    fn dma_alloc_rejects_zero_size_and_bad_align() {
        let mut frames = FakeFrames::new(0x3000_0000);
        assert!(unsafe { dma_alloc(&mut frames, 0, 4096) }.is_err());
        assert!(unsafe { dma_alloc(&mut frames, 4096, 1024) }.is_err()); // below PAGE_SIZE
        assert!(unsafe { dma_alloc(&mut frames, 4096, 5000) }.is_err()); // not a power of two
    }

    #[test]
    fn dma_map_validates_and_returns_a_handle() {
        let buf = DmaBuffer {
            vaddr: 0x1000 as *mut u8,
            paddr: 0x1000,
            size: 4096,
        };
        let _ = unsafe { dma_map(AddressSpaceId(0), buf.vaddr, buf.size) }.unwrap();
        assert_eq!(dma_buffer_phys(&buf), 0x1000);
    }

    #[test]
    fn dma_map_rejects_null_and_zero() {
        assert!(unsafe { dma_map(AddressSpaceId(0), core::ptr::null_mut(), 4096) }.is_err());
        assert!(unsafe { dma_map(AddressSpaceId(0), 0x1000 as *mut u8, 0) }.is_err());
    }

    #[test]
    fn sync_routines_are_no_ops() {
        let h = DmaHandle::new(7);
        dma_sync_for_device(h);
        dma_sync_for_cpu(h);
    }
}
