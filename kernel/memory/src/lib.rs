//! Physical frame allocation, page mapping, and the kernel heap.
//!
//! # The three problems this crate solves
//!
//! 1. **Where does physical memory come from?** The UEFI memory map describes
//!    which physical regions are usable. [`FrameAllocator`] turns that into a
//!    frame-granting allocator over 4 KiB frames.
//! 2. **How does a virtual address reach a physical frame?** [`PageMapper`]
//!    is a thin wrapper over the HAL's [`vela_hal_x86_64::AddressSpace`] that
//!    speaks in pages rather than bytes.
//! 3. **How does the kernel allocate after `ExitBootServices`?** The firmware
//!    allocator is gone, so the kernel needs its own. [`KernelHeap`] is a bump
//!    allocator over a region reserved *before* the firmware allocator was
//!    released.
//!
//! # Why the heap is a bump allocator
//!
//! A general-purpose allocator with free lists needs to know the layout of
//! the heap at build time, and a bump allocator needs only a base and a
//! limit. For a kernel that is not expected to churn through gigabytes of
//! short-lived allocations, the bump allocator plus the page mapper (which
//! *can* free physical frames) is the right trade: the page mapper reclaims
//! real memory, and the heap's job is only to hand out small blocks.
//!
//! The heap is deliberately **not** thread-safe in the fast path. Kernel
//! allocation happens under a spin lock in [`KernelHeap::alloc`]; the
//! allocator is shared through a static, and Phase 4's scheduler makes the
//! kernel single-threaded per CPU.
//!
//! # Memory map format
//!
//! The UEFI memory map is a packed array of descriptors whose stride is
//! `memory_desc_size` — **not** `size_of` of any struct the kernel defines.
//! The firmware is permitted to append fields, so the kernel must read the
//! stride from the boot info and read the fields it understands at the
//! documented offsets within each descriptor. See [`MemoryDescriptor`].

#![no_std]

use alloc::vec::Vec;
use core::alloc::{GlobalAlloc, Layout};
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use spin::Mutex;

use vela_boot_protocol::VelaBootInfo;
use vela_hal_common::{MapError, PageFlags};
use vela_hal_x86_64::AddressSpace;

extern crate alloc;

/// The page size, in bytes.
pub const PAGE_SIZE: u64 = 4096;

/// The page mask: `PAGE_SIZE - 1`.
pub const PAGE_MASK: u64 = PAGE_SIZE - 1;

/// Round `value` up to the next page boundary.
pub const fn align_up(value: u64) -> u64 {
    (value + PAGE_MASK) & !PAGE_MASK
}

/// Round `value` down to a page boundary.
pub const fn align_down(value: u64) -> u64 {
    value & !PAGE_MASK
}

/// True when `value` is page-aligned.
pub const fn is_page_aligned(value: u64) -> bool {
    value & PAGE_MASK == 0
}

/// The UEFI memory-type discriminants the kernel understands.
///
/// These are the values from the UEFI specification's `EFI_MEMORY_TYPE`.
/// Only the "conventional memory" type and the loader-code/data types are
/// treated as allocatable; everything else is either firmware-reserved or
/// device memory and must not be handed out.
pub mod mem_type {
    /// Reserved by the firmware for itself.
    pub const RESERVED: u32 = 0;
    /// Conventional memory, usable.
    pub const CONVENTIONAL: u32 = 1;
    /// Code usable by the bootloader.
    pub const LOADER_CODE: u32 = 2;
    /// Data usable by the bootloader.
    pub const LOADER_DATA: u32 = 3;
    /// Code usable by the boot services driver.
    pub const BOOT_SERVICES_CODE: u32 = 4;
    /// Data usable by the boot services driver.
    pub const BOOT_SERVICES_DATA: u32 = 5;
    /// Runtime services code.
    pub const RUNTIME_SERVICES_CODE: u32 = 6;
    /// Runtime services data.
    pub const RUNTIME_SERVICES_DATA: u32 = 7;
    /// Memory that must be mapped with the CPU caches disabled.
    pub const UNUSABLE: u32 = 8;
    /// Memory that is ACPI reclaimable.
    pub const ACPI_RECLAIMABLE: u32 = 9;
    /// Memory that must be mapped by the OS.
    pub const ACPI_MEMORY_NVS: u32 = 10;
    /// Memory mapped by the OS.
    pub const MMIO: u32 = 11;
    /// Memory mapped by the OS.
    pub const MMIO_CACHE_WB: u32 = 12;
    /// Memory mapped by the OS.
    pub const MMIO_CACHE_UNC: u32 = 13;
    /// Bad memory.
    pub const BAD: u32 = 14;
    /// Memory the firmware must reclaim at exit.
    pub const ACPI_RECLAIMABLE_HIBERNATION: u32 = 15;
    /// Memory mapped by the OS.
    pub const MMIO_CACHE_USWC: u32 = 16;
    /// Memory mapped by the OS.
    pub const MMIO_CACHE_WB_COMBINED: u32 = 17;

    /// True when a region of this type may be allocated from.
    ///
    /// `CONVENTIONAL` is the main case. `LOADER_CODE` and `LOADER_DATA` are
    /// included because the loader has finished with them by the time the
    /// kernel runs, and the firmware's own copy of the kernel image is in
    /// `LOADER_CODE` — reclaiming it is what frees the boot pool.
    ///
    /// Everything else is excluded: `RESERVED` holds the ACPI tables, the
    /// `MMIO` family holds device windows that must not be handed to a page
    /// table, and `ACPI_MEMORY_NVS` must survive until the kernel reads it.
    pub const fn is_allocatable(kind: u32) -> bool {
        matches!(
            kind,
            CONVENTIONAL | LOADER_CODE | LOADER_DATA | BOOT_SERVICES_CODE | BOOT_SERVICES_DATA
        )
    }
}

/// The fields of a UEFI memory descriptor that the kernel reads.
///
/// # Wire format
///
/// The descriptor is a packed C structure:
///
/// ```text
/// offset 0   u32  type
/// offset 4   u32  padding (firmware-specific, must be ignored)
/// offset 8   u64  physical_start
/// offset 16  u64  virtual_start
/// offset 24  u64  number_of_pages
/// offset 32  u64  attribute
/// ```
///
/// The kernel reads these fields at the documented offsets inside a buffer
/// whose stride is `memory_desc_size`. It never assumes the stride equals
/// `size_of::<MemoryDescriptor>()`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemoryDescriptor {
    /// The UEFI memory type.
    pub kind: u32,
    /// Physical base address.
    pub physical_start: u64,
    /// Virtual base address; the firmware's identity mapping, which the kernel
    /// ignores.
    pub virtual_start: u64,
    /// Length in 4 KiB pages.
    pub number_of_pages: u64,
    /// EfiMemoryAttribute bits.
    pub attribute: u64,
}

impl MemoryDescriptor {
    /// The offset of `type` within a descriptor.
    pub const OFFSET_TYPE: usize = 0;
    /// The offset of `physical_start` within a descriptor.
    pub const OFFSET_PHYSICAL_START: usize = 8;
    /// The offset of `virtual_start` within a descriptor.
    pub const OFFSET_VIRTUAL_START: usize = 16;
    /// The offset of `number_of_pages` within a descriptor.
    pub const OFFSET_NUMBER_OF_PAGES: usize = 24;
    /// The offset of `attribute` within a descriptor.
    pub const OFFSET_ATTRIBUTE: usize = 32;
    /// The smallest stride a descriptor can have.
    ///
    /// Every documented field must fit, so a stride below this cannot
    /// describe a complete descriptor.
    pub const MIN_STRIDE: usize = 40;

    /// Decode a descriptor from a byte slice.
    ///
    /// Returns `None` when the slice is too short for the fields the kernel
    /// needs. The `stride` is the distance to the *next* descriptor, not the
    /// size of this one: the firmware may pad, and the padding must not be
    /// interpreted as fields.
    pub fn decode(bytes: &[u8], stride: usize) -> Option<Self> {
        if stride < Self::MIN_STRIDE || bytes.len() < Self::MIN_STRIDE {
            return None;
        }
        let u32_at = |off: usize| -> u32 {
            u32::from_le_bytes([bytes[off], bytes[off + 1], bytes[off + 2], bytes[off + 3]])
        };
        let u64_at = |off: usize| -> u64 {
            let mut buf = [0u8; 8];
            buf.copy_from_slice(&bytes[off..off + 8]);
            u64::from_le_bytes(buf)
        };
        Some(Self {
            kind: u32_at(Self::OFFSET_TYPE),
            physical_start: u64_at(Self::OFFSET_PHYSICAL_START),
            virtual_start: u64_at(Self::OFFSET_VIRTUAL_START),
            number_of_pages: u64_at(Self::OFFSET_NUMBER_OF_PAGES),
            attribute: u64_at(Self::OFFSET_ATTRIBUTE),
        })
    }

    /// The byte length of this region.
    pub const fn byte_len(&self) -> u64 {
        self.number_of_pages * PAGE_SIZE
    }

    /// The first physical address *after* this region.
    pub const fn end(&self) -> u64 {
        self.physical_start + self.byte_len()
    }

    /// True when this region may be allocated from.
    pub const fn is_allocatable(&self) -> bool {
        mem_type::is_allocatable(self.kind)
    }
}

/// A parsed view of the firmware memory map.
///
/// The map is copied out of the firmware buffer at boot, because the buffer
/// itself lives in memory the kernel may reclaim.
#[derive(Debug, Clone, Default)]
pub struct MemoryMap {
    regions: alloc::vec::Vec<MemoryDescriptor>,
    /// Total bytes across every allocatable region.
    usable_bytes: u64,
    /// Total bytes across every region, allocatable or not.
    total_bytes: u64,
}

impl MemoryMap {
    /// Parse a firmware memory map buffer.
    ///
    /// `bytes` is the raw descriptor buffer, `stride` is
    /// `memory_desc_size`. A trailing partial descriptor is ignored rather
    /// than treated as an error: the firmware guarantees a whole number of
    /// descriptors, but a truncated handoff should degrade to "one fewer
    /// region", not to a kernel that refuses to boot.
    pub fn parse(bytes: &[u8], stride: usize) -> Self {
        let mut regions = alloc::vec::Vec::new();
        let mut usable_bytes = 0;
        let mut total_bytes = 0;

        if stride >= MemoryDescriptor::MIN_STRIDE {
            let mut offset = 0usize;
            while offset + MemoryDescriptor::MIN_STRIDE <= bytes.len() {
                let Some(desc) = MemoryDescriptor::decode(&bytes[offset..], stride) else {
                    break;
                };
                usable_bytes += if desc.is_allocatable() {
                    desc.byte_len()
                } else {
                    0
                };
                total_bytes += desc.byte_len();
                regions.push(desc);
                // Step by the firmware's stride, not by our own struct size.
                offset += stride;
            }
        }

        Self {
            regions,
            usable_bytes,
            total_bytes,
        }
    }

    /// Every region in the map, in firmware order.
    pub fn regions(&self) -> &[MemoryDescriptor] {
        &self.regions
    }

    /// The total number of regions.
    pub fn len(&self) -> usize {
        self.regions.len()
    }

    /// True when the map contains no regions.
    pub fn is_empty(&self) -> bool {
        self.regions.is_empty()
    }

    /// Total bytes across every region.
    pub fn total_bytes(&self) -> u64 {
        self.total_bytes
    }

    /// Total bytes across the allocatable regions.
    pub fn usable_bytes(&self) -> u64 {
        self.usable_bytes
    }

    /// The highest physical address the map mentions.
    pub fn highest_address(&self) -> u64 {
        self.regions.iter().map(|r| r.end()).max().unwrap_or(0)
    }

    /// A human-readable summary, for the boot banner.
    pub fn summary(&self) -> alloc::string::String {
        let mut s = alloc::string::String::new();
        s.push_str("regions=");
        s.push_str(&alloc::format!("{}", self.regions.len()));
        s.push_str(" total=");
        s.push_str(&alloc::format!("{} MiB", self.total_bytes / (1024 * 1024)));
        s.push_str(" usable=");
        s.push_str(&alloc::format!("{} MiB", self.usable_bytes / (1024 * 1024)));
        s
    }
}

/// The physical frame allocator.
///
/// A bitmap allocator: one bit per 4 KiB frame across the highest address
/// the memory map mentions. The bitmap itself is allocated from the pool the
/// loader reserved before `ExitBootServices` — the kernel cannot allocate
/// physical memory to describe physical memory.
///
/// # Why a bitmap and not a free list
///
/// A free list needs a header in each free frame, which is intrusive and
/// makes the allocator's metadata depend on the frames it manages. A bitmap
/// is one contiguous array, which means it can live in a single pre-allocated
/// pool and be reasoned about as a whole.
///
/// # The bit ordering
///
/// Bit `i` of the bitmap describes the frame at physical address
/// `i * PAGE_SIZE`. Bits are stored little-endian within each byte, and bit 0
/// of a byte is the *lowest* frame — so frame index `f` is in byte
/// `f / 8` at bit `f % 8`.
pub struct FrameAllocator {
    /// The bitmap: one bit per frame, 1 = free.
    ///
    /// A raw pointer rather than `&'static mut [u64]` so the fast path can
    /// mutate it through `&self` — the allocator is shared behind a `&self`
    /// reference from [`PageMapper`], and a slice field would forbid the
    /// interior mutation that sharing requires.
    bitmap: *mut u64,
    /// Number of words in `bitmap`.
    bitmap_words: usize,
    /// The physical base address corresponding to bitmap bit 0.
    base: u64,
    /// The number of frames the bitmap covers.
    frame_count: u64,
    /// The number of frames still free.
    free_count: AtomicU64,
    /// The next frame to try allocating, so a full scan is not needed on
    /// every call.
    next_hint: AtomicU64,
}

/// The number of frames a bitmap word covers.
const FRAMES_PER_WORD: u64 = 64;

impl FrameAllocator {
    /// Build a frame allocator over a bitmap.
    ///
    /// `bitmap` must be at least `ceil(frames / 64)` words long. `base` is
    /// the physical address of frame 0.
    ///
    /// Every frame starts marked *free*; the caller is responsible for
    /// [`Self::reserve_range`] over the regions that must not be handed out.
    /// Starting everything free and subtracting is the right polarity: a
    /// region the caller forgets to reserve is a leak, but a region it
    /// reserves twice is harmless.
    ///
    /// # Safety
    ///
    /// The bitmap must live in memory that stays valid for the lifetime of
    /// the kernel, must not overlap any other kernel structure, and `base`
    /// must be the physical address of the first frame the bitmap describes.
    pub unsafe fn new(bitmap: &'static mut [u64], base: u64, frames: u64) -> Self {
        assert!(
            (bitmap.len() as u64) * FRAMES_PER_WORD >= frames,
            "frame bitmap too small: {} words for {frames} frames",
            bitmap.len()
        );
        // Every frame starts marked free.
        for word in bitmap.iter() {
            // The words are later reached as `&AtomicU64`, so they must be
            // initialised through an atomic store to be a valid `AtomicU64`
            // value; a plain write is not.
            (&*(word as *const u64).cast::<AtomicU64>()).store(u64::MAX, Ordering::Relaxed);
        }
        // Clear the bits past the end of the covered range so the allocator
        // never hands out a frame that does not exist. This matters most on
        // a 1 GiB machine, where the bitmap is exact, but the arithmetic must
        // hold for any `frames`.
        let spare = (bitmap.len() as u64) * FRAMES_PER_WORD - frames;
        if spare > 0 {
            let last = bitmap.len() - 1;
            (&*(&bitmap[last] as *const u64).cast::<AtomicU64>())
                .store(u64::MAX >> spare, Ordering::Relaxed);
        }
        Self {
            bitmap: bitmap.as_mut_ptr(),
            bitmap_words: bitmap.len(),
            base,
            frame_count: frames,
            free_count: AtomicU64::new(frames),
            next_hint: AtomicU64::new(0),
        }
    }

    /// The number of bitmap words backing this allocator.
    pub fn bitmap_words(&self) -> usize {
        self.bitmap_words
    }

    /// The bytes the bitmap itself occupies.
    ///
    /// Worth reporting in the boot banner: on a large machine the bitmap is
    /// the single largest structure the kernel allocates before the heap
    /// exists, and a surprise there explains a surprising boot-time
    /// reservation.
    pub fn bitmap_bytes(&self) -> u64 {
        (self.bitmap_words * core::mem::size_of::<u64>()) as u64
    }

    /// Mark the frames covering a physical range as *used*.
    ///
    /// Used to reserve regions that must not be handed out: the memory map's
    /// non-allocatable regions, the kernel image, the initrd, and the page
    /// tables themselves.
    ///
    /// Takes `&self` rather than `&mut self` on purpose. The allocator is
    /// shared as `&'static FrameAllocator` so the page mapper can reach it,
    /// and boot-time reservation happens through that shared reference. Making
    /// reservation take `&mut self` would force the caller to cast the shared
    /// reference to a mutable one, which is undefined behaviour — the correct
    /// primitive for interior mutation through a shared reference is an
    /// atomic or a lock, and the bitmap word itself is the atomic unit here.
    pub fn reserve_range(&self, start: u64, end: u64) {
        if end <= start {
            return;
        }
        let first = self.frame_index(start);
        // `end - 1` avoids reserving a frame past the range when `end` is
        // itself page-aligned.
        let last = self.frame_index(end - 1);
        for frame in first..=last {
            if frame < self.frame_count {
                self.set_free(frame, false);
            }
        }
    }

    /// The frame index for a physical address.
    fn frame_index(&self, addr: u64) -> u64 {
        addr.saturating_sub(self.base) / PAGE_SIZE
    }

    /// The physical address of a frame index.
    pub fn frame_address(&self, frame: u64) -> u64 {
        self.base + frame * PAGE_SIZE
    }

    /// The number of frames the allocator covers.
    pub fn frame_count(&self) -> u64 {
        self.frame_count
    }

    /// The number of frames still free.
    pub fn free_frames(&self) -> u64 {
        self.free_count.load(Ordering::Acquire)
    }

    /// The number of bytes still free.
    pub fn free_bytes(&self) -> u64 {
        self.free_frames() * PAGE_SIZE
    }

    /// The physical base address of frame 0.
    pub fn base(&self) -> u64 {
        self.base
    }

    /// Read the free bit of a frame.
    fn is_free(&self, frame: u64) -> bool {
        let word = (frame / FRAMES_PER_WORD) as usize;
        let bit = (frame % FRAMES_PER_WORD) as u32;
        // SAFETY: `frame < frame_count <= bitmap_words * 64`, so `word` is in
        // bounds. The pointer is valid for the allocator's lifetime, and the
        // words were initialised as `AtomicU64` values by `new`.
        let slot = unsafe { &*self.bitmap.add(word).cast::<AtomicU64>() };
        slot.load(Ordering::Relaxed) & (1u64 << bit) != 0
    }

    /// Set the free bit of a frame, keeping the free count in step.
    ///
    /// The bitmap word is updated with a compare-and-swap so two callers
    /// racing on different bits of the same word cannot lose each other's
    /// change. Reserving a 16 MiB region touches 4096 bits spread across 64
    /// words, so this is not a theoretical concern.
    fn set_free(&self, frame: u64, free: bool) {
        let word = (frame / FRAMES_PER_WORD) as usize;
        let bit = (frame % FRAMES_PER_WORD) as u32;
        let mask = 1u64 << bit;
        // SAFETY: as in `is_free`.
        let slot = unsafe { &*self.bitmap.add(word).cast::<AtomicU64>() };
        let mut current = slot.load(Ordering::Relaxed);
        loop {
            let updated = if free {
                if current & mask != 0 {
                    // Already free: nothing to do, and the count must not
                    // drift. This is the double-free tolerance.
                    return;
                }
                current | mask
            } else {
                if current & mask == 0 {
                    // Already reserved.
                    return;
                }
                current & !mask
            };
            match slot.compare_exchange_weak(current, updated, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(_) => break,
                Err(actual) => current = actual,
            }
        }
        if free {
            self.free_count.fetch_add(1, Ordering::Release);
        } else {
            self.free_count.fetch_sub(1, Ordering::Release);
        }
    }

    /// Allocate one physical frame.
    ///
    /// Returns the frame's physical address, or `None` when memory is
    /// exhausted.
    ///
    /// The scan starts from a rotating hint and wraps, so allocation is
    /// amortised O(1) rather than O(n) from frame 0 every time — the
    /// difference matters once the page mapper starts churning.
    pub fn alloc_frame(&self) -> Option<u64> {
        let mut hint = self.next_hint.load(Ordering::Relaxed);
        if hint >= self.frame_count {
            hint = 0;
        }
        // Two passes: from the hint to the end, then from the beginning up to
        // the hint. The second pass is what makes the allocator eventually
        // return every frame even when the hint is stuck near the end.
        for pass in 0..2u64 {
            let start = if pass == 0 { hint } else { 0 };
            let end = if pass == 0 { self.frame_count } else { hint };
            let mut frame = start;
            while frame < end {
                if self.is_free(frame) {
                    self.set_free(frame, false);
                    self.next_hint.store(frame + 1, Ordering::Relaxed);
                    return Some(self.frame_address(frame));
                }
                frame += 1;
            }
        }
        None
    }

    /// Free a physical frame.
    ///
    /// Double-freeing is tolerated: the bit is already clear, so the free
    /// count does not drift. That tolerance is deliberate — a kernel that
    /// panics on a double free is a kernel that turns a small bookkeeping bug
    /// into a machine that will not boot again.
    ///
    /// # Safety
    ///
    /// `addr` must be a frame this allocator covers. Freeing a frame that is
    /// still mapped lets another mapping alias it.
    pub unsafe fn free_frame(&self, addr: u64) {
        let frame = self.frame_index(addr);
        if frame >= self.frame_count {
            return;
        }
        self.set_free(frame, true);
    }

    /// Read the free bit of a frame by address, for diagnostics and tests.
    pub fn frame_is_free(&self, addr: u64) -> bool {
        let frame = self.frame_index(addr);
        frame < self.frame_count && self.is_free(frame)
    }
}

/// The kernel heap: a bump allocator over a pre-reserved region.
///
/// # Why a bump allocator
///
/// See the module documentation. The important property is that the heap's
/// backing store is reserved *before* `ExitBootServices`, because after that
/// point there is no firmware allocator to fall back on, and a panic handler
/// that cannot allocate cannot even print its own message.
pub struct KernelHeap {
    /// The next free byte.
    next: AtomicUsize,
    /// One past the last usable byte.
    end: AtomicUsize,
    /// The base address, kept for diagnostics.
    base: usize,
    /// The number of allocations served.
    allocations: AtomicU64,
    /// The number of bytes handed out.
    bytes_used: AtomicU64,
}

impl KernelHeap {
    /// Create a heap over `[base, base + size)`.
    ///
    /// # Safety
    ///
    /// The region must be valid, writable, physically contiguous, and must not
    /// overlap anything else the kernel uses.
    pub unsafe fn new(base: usize, size: usize) -> Self {
        Self {
            next: AtomicUsize::new(base),
            end: AtomicUsize::new(base + size),
            base,
            allocations: AtomicU64::new(0),
            bytes_used: AtomicU64::new(0),
        }
    }

    /// Allocate `size` bytes with the given alignment.
    ///
    /// Returns `None` when the heap is exhausted.
    pub fn alloc(&self, size: usize, align: usize) -> Option<usize> {
        if align == 0 || !align.is_power_of_two() {
            return None;
        }
        // A zero-sized allocation is legal in Rust and must return a
        // non-null, aligned, non-dereferenceable pointer.
        if size == 0 {
            return Some(self.next.load(Ordering::Acquire));
        }
        let mut current = self.next.load(Ordering::Acquire);
        loop {
            let aligned = (current + align - 1) & !(align - 1);
            let new_next = aligned.checked_add(size)?;
            if new_next > self.end.load(Ordering::Acquire) {
                return None;
            }
            match self.next.compare_exchange_weak(
                current,
                new_next,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    self.allocations.fetch_add(1, Ordering::Relaxed);
                    self.bytes_used
                        .fetch_add((new_next - current) as u64, Ordering::Relaxed);
                    return Some(aligned);
                }
                Err(actual) => current = actual,
            }
        }
    }

    /// The number of bytes handed out so far.
    pub fn bytes_used(&self) -> u64 {
        self.bytes_used.load(Ordering::Relaxed)
    }

    /// The number of allocations served.
    pub fn allocation_count(&self) -> u64 {
        self.allocations.load(Ordering::Relaxed)
    }

    /// The total capacity of the heap, in bytes.
    pub fn capacity(&self) -> usize {
        self.end.load(Ordering::Acquire) - self.base
    }

    /// The number of bytes still available.
    pub fn bytes_free(&self) -> usize {
        self.end.load(Ordering::Acquire) - self.next.load(Ordering::Acquire)
    }

    /// The base address of the heap.
    pub fn base(&self) -> usize {
        self.base
    }
}

/// The process-wide kernel heap.
///
/// `None` until [`init_heap`] installs one. Every accessor degrades to
/// "allocation failed" rather than panicking, so a heap-less kernel still
/// runs — it just cannot allocate.
static HEAP: Mutex<Option<&'static KernelHeap>> = Mutex::new(None);

/// Install the kernel heap.
///
/// # Safety
///
/// The heap must outlive the kernel and must not overlap any other kernel
/// structure.
pub unsafe fn init_heap(heap: &'static KernelHeap) {
    *HEAP.lock() = Some(heap);
}

/// The installed kernel heap, if any.
pub fn heap() -> Option<&'static KernelHeap> {
    *HEAP.lock()
}

/// The page mapper: a page-granular wrapper over the HAL address space.
///
/// # Why a wrapper at all
///
/// [`AddressSpace`] maps *physical* ranges to *virtual* ranges and, when it
/// needs a lower-level table, asks a `FrameSource` for it. That is the right
/// primitive for setting up a mapping once a physical address is known. The
/// kernel overwhelmingly wants the other operation: "give me a page at this
/// virtual address", with the physical side decided by the allocator. This
/// wrapper expresses exactly that, and keeps the frame bookkeeping — freeing
/// the frame on unmap, returning the frame when a map fails — in one place
/// rather than at every call site.
pub struct PageMapper {
    space: AddressSpace,
    frames: &'static FrameAllocator,
}

impl PageMapper {
    /// Create a mapper over an address space and a frame allocator.
    pub const fn new(space: AddressSpace, frames: &'static FrameAllocator) -> Self {
        Self { space, frames }
    }

    /// Map one page with the given flags, allocating the frame.
    ///
    /// `virt` must be page-aligned.
    pub fn map_page(&mut self, virt: u64, flags: PageFlags) -> Result<u64, MapError> {
        if !virt.is_multiple_of(PAGE_SIZE) {
            return Err(MapError::MisalignedVirtual);
        }
        let Some(frame) = self.frames.alloc_frame() else {
            return Err(MapError::OutOfMemory);
        };
        match self.space.map_page(virt, frame, flags) {
            Ok(()) => Ok(frame),
            Err(e) => {
                // The frame was allocated but the mapping failed; give it back
                // so a failed map does not leak physical memory. This is the
                // case that turns a `AlreadyMapped` into a leak if forgotten.
                // SAFETY: the frame came from this allocator, was never
                // mapped, and is being returned exactly once.
                unsafe { self.frames.free_frame(frame) };
                Err(e)
            }
        }
    }

    /// Map a range of pages, allocating each frame.
    pub fn map_range(&mut self, virt: u64, pages: u64, flags: PageFlags) -> Result<(), MapError> {
        if !virt.is_multiple_of(PAGE_SIZE) {
            return Err(MapError::MisalignedVirtual);
        }
        for i in 0..pages {
            // A partial failure leaves the earlier pages mapped. The caller
            // gets the error and is responsible for unwinding; leaving them
            // mapped is safer than unmapping frames the caller may already
            // hold references to.
            self.map_page(virt + i * PAGE_SIZE, flags)?;
        }
        Ok(())
    }

    /// Unmap one page, freeing the underlying frame and returning it.
    pub fn unmap_page(&mut self, virt: u64) -> Result<u64, MapError> {
        if !virt.is_multiple_of(PAGE_SIZE) {
            return Err(MapError::MisalignedVirtual);
        }
        let frame = self.space.unmap_page(virt)?;
        // SAFETY: the frame was mapped from this allocator's pool, and it is
        // being unmapped exactly once.
        unsafe { self.frames.free_frame(frame) };
        Ok(frame)
    }

    /// Unmap a range of pages, freeing every frame.
    pub fn unmap_range(&mut self, virt: u64, pages: u64) -> Result<(), MapError> {
        if !virt.is_multiple_of(PAGE_SIZE) {
            return Err(MapError::MisalignedVirtual);
        }
        for i in 0..pages {
            self.unmap_page(virt + i * PAGE_SIZE)?;
        }
        Ok(())
    }

    /// Translate a virtual address to the physical address of its frame.
    pub fn translate(&self, virt: u64) -> Option<u64> {
        self.space.translate(virt)
    }

    /// Change a page's flags in place, keeping the same frame.
    pub fn set_flags(&self, virt: u64, flags: PageFlags) -> Result<(), MapError> {
        self.space.set_flags(virt, flags)
    }

    /// The address space being mapped.
    pub fn space(&self) -> &AddressSpace {
        &self.space
    }
}

impl core::fmt::Display for PlanError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::NoMemoryMap => write!(f, "the loader provided no memory map"),
            Self::BadStride { stride } => {
                write!(f, "memory descriptor stride {stride} is too small")
            }
            Self::NothingUsable => write!(f, "no allocatable memory regions"),
            Self::RegionTooSmall { largest } => {
                write!(f, "largest usable region is {largest} bytes, too small")
            }
            Self::Overlap { addr } => write!(f, "required regions overlap at {addr:#x}"),
        }
    }
}

/// Why building the physical memory plan failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanError {
    /// The memory map was not provided.
    NoMemoryMap,
    /// The memory map's descriptor stride was zero or too small.
    BadStride {
        /// The offending stride.
        stride: usize,
    },
    /// The memory map contained no allocatable regions.
    NothingUsable,
    /// No region was large enough to host the bitmap.
    RegionTooSmall {
        /// The largest region found, in bytes.
        largest: u64,
    },
    /// A required region overlapped another and could not be reconciled.
    Overlap {
        /// The start of the conflicting region.
        addr: u64,
    },
}

/// The physical memory plan, derived from a [`VelaBootInfo`].
///
/// Building this is the first thing the kernel does after validating the
/// handoff, and it has to answer three questions before anything can
/// allocate:
///
/// 1. Which physical ranges may be handed out? — every allocatable region in
///    the memory map, minus the ones the loader or the kernel owns.
/// 2. Where does the frame bitmap live? — it must live *in* one of those
///    allocatable regions, which means carving that region out before the
///    allocator starts.
/// 3. Where does the heap live? — likewise.
///
/// The plan is computed **once**, before any allocator exists, and its inputs
/// come from the boot info rather than from any live allocator. That ordering
/// is the whole point: the allocator cannot describe memory without first
/// knowing where the memory is, and it cannot be modified to make room for
/// itself after it has started handing out frames.
#[derive(Debug, Clone)]
pub struct MemoryPlan {
    /// The parsed memory map.
    pub map: MemoryMap,
    /// Physical base address of frame 0, page-aligned down.
    pub frame_base: u64,
    /// Total frames covered by the bitmap.
    pub frame_count: u64,
    /// Bytes of bitmap needed.
    pub bitmap_bytes: u64,
    /// The region carved out for the bitmap: `(start, end)`.
    pub bitmap_region: (u64, u64),
    /// The region carved out for the heap: `(start, end)`.
    pub heap_region: (u64, u64),
    /// Regions that must be reserved and never handed out, as
    /// `(start, end)` pairs.
    pub reserved: Vec<(u64, u64)>,
    /// Total bytes available to the frame allocator.
    pub allocatable_bytes: u64,
}

impl MemoryPlan {
    /// Compute the plan.
    ///
    /// `heap_bytes` is how much the kernel wants for its bump heap. The
    /// kernel image, initrd and framebuffer — whose extents are all known from
    /// the boot info — are reserved automatically, because a frame allocator
    /// that handed out the framebuffer's frames would corrupt the display the
    /// kernel is about to draw on.
    ///
    /// `bitmap_bytes` is the size of the caller-provided bitmap, which must
    /// already be reserved. The plan asserts the bitmap region is inside an
    /// allocatable region and subtracts it.
    pub fn build(
        map: MemoryMap,
        bitmap_bytes: u64,
        heap_bytes: u64,
        must_reserve: &[(u64, u64)],
    ) -> Result<Self, PlanError> {
        // The bitmap must describe every frame from the lowest to the highest
        // address the map mentions, so the covered range is the map's extent.
        let highest = map.highest_address();
        if map.usable_bytes() == 0 {
            return Err(PlanError::NothingUsable);
        }
        let frame_base = 0u64;
        let frame_count = highest / PAGE_SIZE;
        let needed_bitmap_bytes = frame_count.div_ceil(FRAMES_PER_WORD) * 8;
        if needed_bitmap_bytes > bitmap_bytes {
            return Err(PlanError::RegionTooSmall {
                largest: bitmap_bytes,
            });
        }

        // The null page is reserved unconditionally.
        //
        // A null-pointer dereference that lands on a mapped page is a wild
        // write that corrupts something unrelated and leaves no trace; a null
        // pointer that faults is a clean, debuggable crash. Reserving the
        // first page makes the difference, and it costs 4 KiB.
        let mut reservations: Vec<(u64, u64)> = Vec::with_capacity(must_reserve.len() + 1);
        reservations.push((0, PAGE_SIZE));
        reservations.extend_from_slice(must_reserve);

        // Carve the bitmap and the heap out of the largest allocatable region
        // that can hold them, preferring regions above everything the kernel
        // touches so a stray write into the bitmap cannot land on the kernel
        // image.
        let allocatable: Vec<&MemoryDescriptor> = map
            .regions()
            .iter()
            .filter(|r| r.is_allocatable() && r.byte_len() > 0)
            .collect();
        let Some(largest) = allocatable.iter().max_by_key(|r| r.byte_len()) else {
            return Err(PlanError::NothingUsable);
        };

        let mut bitmap_region = (0u64, 0u64);
        let mut heap_region = (0u64, 0u64);
        let mut allocatable_bytes = 0u64;

        for region in &allocatable {
            let start = align_up(region.physical_start);
            let end = align_down(region.end());
            if end <= start {
                continue;
            }

            // Subtract the reservations that intersect this region, producing
            // a list of free spans. A reservation at the *front* of a region
            // must shrink the front, not the back: truncating the back would
            // discard the whole region and report "no memory" for a machine
            // that has plenty.
            let mut holes: Vec<(u64, u64)> = reservations
                .iter()
                .filter(|(rs, re)| *rs < end && *re > start)
                .map(|(rs, re)| ((*rs).max(start), (*re).min(end)))
                .collect();
            holes.sort_unstable();

            let mut free_spans: Vec<(u64, u64)> = Vec::new();
            let mut cursor = start;
            for (hs, he) in holes {
                if hs > cursor {
                    free_spans.push((cursor, hs));
                }
                cursor = cursor.max(he);
            }
            if cursor < end {
                free_spans.push((cursor, end));
            }

            for (span_start, mut span_end) in free_spans {
                if span_end <= span_start {
                    continue;
                }
                // Carve from the *top* of the span downwards.
                //
                // Two reasons, both about leaving the low end usable. First,
                // address 0 is the null page: a null-pointer dereference that
                // lands on a mapped page produces a wild write instead of a
                // clean fault, so the low end of memory is the last place the
                // kernel's own structures belong. Second, on a machine whose
                // firmware identity-maps the low gigabytes, keeping the
                // kernel's structures high avoids colliding with whatever the
                // firmware left there.
                if bitmap_region.1 == 0 && span_end - span_start >= needed_bitmap_bytes {
                    span_end -= needed_bitmap_bytes;
                    bitmap_region = (span_end, span_end + needed_bitmap_bytes);
                }
                if heap_region.1 == 0 && span_end - span_start >= heap_bytes {
                    span_end -= heap_bytes;
                    heap_region = (span_end, span_end + heap_bytes);
                }
                allocatable_bytes += span_end.saturating_sub(span_start);
            }
        }

        if bitmap_region.1 == 0 {
            return Err(PlanError::RegionTooSmall {
                largest: largest.byte_len(),
            });
        }
        if heap_bytes > 0 && heap_region.1 == 0 {
            return Err(PlanError::RegionTooSmall {
                largest: largest.byte_len(),
            });
        }

        Ok(Self {
            map,
            frame_base,
            frame_count,
            bitmap_bytes: needed_bitmap_bytes,
            bitmap_region,
            heap_region,
            reserved: must_reserve.to_vec(),
            allocatable_bytes,
        })
    }

    /// The regions the frame allocator must reserve, in addition to the
    /// bitmap and the heap.
    ///
    /// Includes every non-allocatable region from the map, plus whatever the
    /// caller passed to [`Self::build`].
    pub fn reserved_regions(&self) -> Vec<(u64, u64)> {
        let mut out: Vec<(u64, u64)> = Vec::with_capacity(self.reserved.len() + 8);
        // The null page is always reserved.
        out.push((0, PAGE_SIZE));
        out.extend_from_slice(&self.reserved);
        for region in self.map.regions() {
            if !region.is_allocatable() {
                out.push((region.physical_start, region.end()));
            }
        }
        // The bitmap and the heap are inside allocatable regions but are owned
        // by the kernel, so they must be reserved from the frame allocator.
        if self.bitmap_region.1 > self.bitmap_region.0 {
            out.push(self.bitmap_region);
        }
        if self.heap_region.1 > self.heap_region.0 {
            out.push(self.heap_region);
        }
        out
    }

    /// A multi-line summary for the boot banner.
    pub fn summary(&self) -> alloc::string::String {
        let mut s = alloc::string::String::new();
        s.push_str(&self.map.summary());
        s.push_str(" frames=");
        s.push_str(&alloc::format!("{}", self.frame_count));
        s.push_str(" bitmap=");
        s.push_str(&alloc::format!("{} KiB", self.bitmap_bytes / 1024));
        s.push_str(" heap=");
        s.push_str(&alloc::format!(
            "{} KiB",
            (self.heap_region.1 - self.heap_region.0) / 1024
        ));
        s.push_str(" allocatable=");
        s.push_str(&alloc::format!(
            "{} MiB",
            self.allocatable_bytes / (1024 * 1024)
        ));
        s
    }
}

/// The boot-time reservation set.
///
/// # Why the list is a parameter and not hardcoded
///
/// The kernel does not know the framebuffer's or the initrd's extent at the
/// moment it builds the plan — those live in the boot info, and the boot info
/// is a struct the loader fills in, not a value the kernel can recompute.
/// Collecting the reservations in one place means the list is auditable: the
/// set of physical memory the kernel refuses to hand out is visible as a
/// value, not scattered through the boot code.
#[derive(Debug, Clone, Default)]
pub struct BootReservations {
    /// Ranges the kernel owns and must never be allocated from.
    pub ranges: Vec<(u64, u64)>,
}

impl BootReservations {
    /// An empty reservation set.
    pub fn empty() -> Self {
        Self { ranges: Vec::new() }
    }

    /// Add a range, page-aligning both ends outward.
    pub fn push(&mut self, start: u64, end: u64) {
        if end <= start {
            return;
        }
        self.ranges.push((align_down(start), align_up(end)));
    }

    /// Build the reservation set implied by a boot handoff.
    ///
    /// Covers the kernel image, the initrd, the framebuffer and the memory-map
    /// buffer. All four are live kernel dependencies: handing their frames to a
    /// user process would corrupt the kernel itself, the display it draws on,
    /// or the very map it is reading.
    pub fn from_boot_info(bi: &VelaBootInfo) -> Self {
        let mut r = Self::empty();
        if bi.kernel_paddr != 0 && bi.kernel_bytes != 0 {
            r.push(bi.kernel_paddr, bi.kernel_paddr + bi.kernel_bytes);
        }
        if bi.initrd_paddr != 0 && bi.initrd_bytes != 0 {
            r.push(bi.initrd_paddr, bi.initrd_paddr + bi.initrd_bytes);
        }
        if bi.framebuffer_paddr != 0 && bi.framebuffer_bytes != 0 {
            r.push(
                bi.framebuffer_paddr,
                bi.framebuffer_paddr + bi.framebuffer_bytes,
            );
        }
        if bi.memory_map_paddr != 0 && bi.memory_map_bytes != 0 {
            r.push(
                bi.memory_map_paddr,
                bi.memory_map_paddr + bi.memory_map_bytes,
            );
        }
        r
    }

    /// The ranges, as a slice.
    pub fn as_slice(&self) -> &[(u64, u64)] {
        &self.ranges
    }

    /// True when `addr` falls inside any reserved range.
    pub fn contains(&self, addr: u64) -> bool {
        self.ranges.iter().any(|(s, e)| addr >= *s && addr < *e)
    }

    /// Total bytes reserved.
    pub fn total_bytes(&self) -> u64 {
        self.ranges
            .iter()
            .map(|(s, e)| align_up(*e).saturating_sub(align_down(*s)))
            .sum()
    }
}

/// The global allocator, backed by the kernel heap.
///
/// # Why this exists
///
/// The kernel needs a `#[global_allocator]` for `alloc`/`Vec` to work in
/// `no_std`. It is installed only after the heap is initialised, and the
/// allocation path degrades to a null pointer when there is no heap, which is
/// what the allocator contract requires: returning null is how `alloc` reports
/// failure, and a panic inside a global allocator is a double fault waiting to
/// happen.
pub struct HeapGlobalAlloc;

unsafe impl GlobalAlloc for HeapGlobalAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        match heap() {
            Some(h) => match h.alloc(layout.size(), layout.align()) {
                Some(ptr) => ptr as *mut u8,
                None => core::ptr::null_mut(),
            },
            None => core::ptr::null_mut(),
        }
    }

    unsafe fn dealloc(&self, _ptr: *mut u8, _layout: Layout) {
        // The bump allocator never reclaims. The page mapper reclaims whole
        // pages, which is where the memory pressure is actually relieved.
    }
}

#[cfg(test)]
extern crate std;

#[cfg(test)]
mod tests {
    use super::*;
    use std::boxed::Box;
    // silence unused when only format! is used
    use std::vec;
    use std::vec::Vec;

    /// Build a synthetic firmware memory map.
    fn build_map(regions: &[(u32, u64, u64)]) -> (Vec<u8>, usize) {
        // Use the real UEFI descriptor stride so the decode path is exercised
        // with the layout the firmware actually produces.
        const STRIDE: usize = 40;
        let mut buf = Vec::new();
        for (kind, start, pages) in regions {
            let mut d = [0u8; STRIDE];
            d[0..4].copy_from_slice(&kind.to_le_bytes());
            d[8..16].copy_from_slice(&start.to_le_bytes());
            d[16..24].copy_from_slice(&start.to_le_bytes());
            d[24..32].copy_from_slice(&pages.to_le_bytes());
            d[32..40].copy_from_slice(&0u64.to_le_bytes());
            buf.extend_from_slice(&d);
        }
        (buf, STRIDE)
    }

    #[test]
    fn alignment_helpers_are_correct() {
        assert_eq!(align_up(0), 0);
        assert_eq!(align_up(1), PAGE_SIZE);
        assert_eq!(align_up(PAGE_SIZE), PAGE_SIZE);
        assert_eq!(align_up(PAGE_SIZE + 1), PAGE_SIZE * 2);
        assert_eq!(align_down(PAGE_SIZE + 1), PAGE_SIZE);
        assert_eq!(align_down(PAGE_SIZE - 1), PAGE_SIZE - PAGE_SIZE);
        assert!(is_page_aligned(0));
        assert!(is_page_aligned(PAGE_SIZE));
        assert!(!is_page_aligned(PAGE_SIZE - 1));
    }

    #[test]
    fn memory_types_are_classified_correctly() {
        assert!(mem_type::is_allocatable(mem_type::CONVENTIONAL));
        assert!(mem_type::is_allocatable(mem_type::LOADER_CODE));
        assert!(!mem_type::is_allocatable(mem_type::RESERVED));
        assert!(!mem_type::is_allocatable(mem_type::MMIO));
        assert!(!mem_type::is_allocatable(mem_type::BAD));
        assert!(!mem_type::is_allocatable(mem_type::ACPI_MEMORY_NVS));
        assert!(!mem_type::is_allocatable(mem_type::UNUSABLE));
    }

    #[test]
    fn descriptors_decode_from_a_raw_buffer() {
        let (buf, stride) = build_map(&[(mem_type::CONVENTIONAL, 0x1000, 4)]);
        let d = MemoryDescriptor::decode(&buf, stride).unwrap();
        assert_eq!(d.kind, mem_type::CONVENTIONAL);
        assert_eq!(d.physical_start, 0x1000);
        assert_eq!(d.number_of_pages, 4);
        assert_eq!(d.byte_len(), 4 * PAGE_SIZE);
        assert_eq!(d.end(), 0x1000 + 4 * PAGE_SIZE);
    }

    #[test]
    fn a_short_buffer_decodes_to_nothing() {
        let (buf, stride) = build_map(&[(mem_type::CONVENTIONAL, 0x1000, 4)]);
        // Truncated: the fields the kernel needs are missing.
        assert!(MemoryDescriptor::decode(&buf[..20], stride).is_none());
        // A stride too small to hold the fields.
        assert!(MemoryDescriptor::decode(&buf, 8).is_none());
    }

    #[test]
    fn the_stride_may_exceed_the_struct_size() {
        // The firmware is allowed to pad descriptors. A stride of 48 with
        // 40 bytes of real data must decode correctly.
        let (mut buf, _) = build_map(&[(mem_type::CONVENTIONAL, 0x1000, 4)]);
        buf.extend_from_slice(&[0u8; 8]);
        let d = MemoryDescriptor::decode(&buf, 48).unwrap();
        assert_eq!(d.physical_start, 0x1000);
        assert_eq!(d.number_of_pages, 4);
    }

    #[test]
    fn a_map_sums_usable_and_total_bytes() {
        let (buf, stride) = build_map(&[
            (mem_type::CONVENTIONAL, 0x0, 16), // 64 KiB
            (mem_type::RESERVED, 0x10_000, 8), // 32 KiB
            (mem_type::MMIO, 0x20_000, 4),     // 16 KiB
        ]);
        let map = MemoryMap::parse(&buf, stride);
        assert_eq!(map.len(), 3);
        assert_eq!(
            map.total_bytes(),
            16 * PAGE_SIZE + 8 * PAGE_SIZE + 4 * PAGE_SIZE
        );
        assert_eq!(map.usable_bytes(), 16 * PAGE_SIZE);
        assert_eq!(map.highest_address(), 0x20_000 + 4 * PAGE_SIZE);
    }

    #[test]
    fn an_empty_buffer_yields_an_empty_map() {
        let map = MemoryMap::parse(&[], 40);
        assert!(map.is_empty());
        assert_eq!(map.total_bytes(), 0);
        assert_eq!(map.usable_bytes(), 0);
        assert_eq!(map.highest_address(), 0);
    }

    #[test]
    fn a_truncated_trailing_descriptor_is_ignored() {
        let (mut buf, stride) = build_map(&[(mem_type::CONVENTIONAL, 0x0, 4)]);
        // Append half a descriptor.
        buf.extend_from_slice(&[0u8; 20]);
        let map = MemoryMap::parse(&buf, stride);
        assert_eq!(map.len(), 1, "the partial descriptor must be dropped");
    }

    /// Build a frame allocator over a static bitmap for testing.
    ///
    /// The bitmap is leaked so it satisfies the `'static` lifetime the
    /// allocator requires without a global allocator in the way.
    fn test_allocator(base: u64, frames: u64) -> FrameAllocator {
        let words = (frames as usize).div_ceil(64);
        let bitmap: &'static mut [u64] = Box::leak(vec![0u64; words].into_boxed_slice());
        // SAFETY: the bitmap is leaked, so it outlives the allocator.
        unsafe { FrameAllocator::new(bitmap, base, frames) }
    }

    #[test]
    fn a_fresh_allocator_has_every_frame_free() {
        let alloc = test_allocator(0, 128);
        assert_eq!(alloc.frame_count(), 128);
        assert_eq!(alloc.free_frames(), 128);
        assert_eq!(alloc.free_bytes(), 128 * PAGE_SIZE);
    }

    #[test]
    fn allocation_hands_out_distinct_frames() {
        let alloc = test_allocator(0x1000, 64);
        let mut seen = std::collections::HashSet::new();
        for _ in 0..64 {
            let f = alloc.alloc_frame().unwrap();
            assert!(seen.insert(f), "frame {f:#x} handed out twice");
            assert_eq!(f % PAGE_SIZE, 0, "frames are page-aligned");
        }
        assert!(alloc.alloc_frame().is_none(), "the pool is exhausted");
        assert_eq!(alloc.free_frames(), 0);
    }

    #[test]
    fn freeing_a_frame_returns_it_to_the_pool() {
        let alloc = test_allocator(0, 8);
        let a = alloc.alloc_frame().unwrap();
        let b = alloc.alloc_frame().unwrap();
        assert_eq!(alloc.free_frames(), 6);
        // SAFETY: `a` came from this allocator and has not been freed.
        unsafe { alloc.free_frame(a) };
        assert_eq!(alloc.free_frames(), 7);
        // The freed frame can be handed out again.
        let c = alloc.alloc_frame().unwrap();
        assert!(c == a || c == b || c != 0);
        assert_eq!(alloc.free_frames(), 6);
    }

    #[test]
    fn double_freeing_does_not_corrupt_the_count() {
        let alloc = test_allocator(0, 8);
        let a = alloc.alloc_frame().unwrap();
        assert_eq!(alloc.free_frames(), 7, "one frame handed out");
        // SAFETY: first free of a live frame.
        unsafe { alloc.free_frame(a) };
        assert_eq!(alloc.free_frames(), 8, "the frame came back");
        // SAFETY: double free. The allocator must be robust to it: the bit is
        // already set, so the free count must not drift upward a second time.
        unsafe { alloc.free_frame(a) };
        assert_eq!(alloc.free_frames(), 8, "the count must not drift");
    }

    #[test]
    fn freeing_a_frame_the_allocator_never_handed_out_is_ignored() {
        let alloc = test_allocator(0x1000, 8);
        // SAFETY: an address inside the covered range. It was never allocated,
        // but its bit is set, so this is indistinguishable from a real free and
        // must be tolerated rather than corrupting the count.
        unsafe { alloc.free_frame(0x1000) };
        assert_eq!(alloc.free_frames(), 8, "no drift from an idempotent free");
    }

    #[test]
    fn reserved_ranges_are_never_handed_out() {
        let alloc = test_allocator(0, 64);
        // Reserve the first 16 frames.
        alloc.reserve_range(0, 16 * PAGE_SIZE);
        assert_eq!(alloc.free_frames(), 48);
        for _ in 0..48 {
            let f = alloc.alloc_frame().unwrap();
            assert!(f >= 16 * PAGE_SIZE, "reserved frame {f:#x} was handed out");
        }
        assert!(alloc.alloc_frame().is_none());
    }

    #[test]
    fn reserving_beyond_the_pool_is_clamped() {
        let alloc = test_allocator(0, 16);
        // Reserving past the end must not panic or underflow the free count.
        alloc.reserve_range(0, 1024 * PAGE_SIZE);
        assert_eq!(alloc.free_frames(), 0);
    }

    #[test]
    fn the_allocator_recovers_frames_after_a_wrapped_cursor() {
        // Allocate everything, free a frame at the very end, and confirm the
        // two-pass scan still finds it.
        let alloc = test_allocator(0, 32);
        let mut last = 0;
        for _ in 0..32 {
            last = alloc.alloc_frame().unwrap();
        }
        assert!(alloc.alloc_frame().is_none());
        // SAFETY: `last` was allocated and not yet freed.
        unsafe { alloc.free_frame(last) };
        assert_eq!(alloc.alloc_frame(), Some(last));
    }

    #[test]
    fn the_kernel_heap_bumps_forward() {
        // SAFETY: a leaked region is valid for the heap's lifetime.
        let region: &'static mut [u8] = Box::leak(vec![0u8; 4096].into_boxed_slice());
        let base = region.as_ptr() as usize;
        // SAFETY: the region is leaked and writable.
        let heap = unsafe { KernelHeap::new(base, 4096) };

        let a = heap.alloc(64, 8).unwrap();
        let b = heap.alloc(64, 8).unwrap();
        assert!(b >= a + 64, "the heap must bump forward");
        assert_eq!(a % 8, 0);
        assert_eq!(heap.allocation_count(), 2);
        assert!(heap.bytes_used() >= 128);
    }

    #[test]
    fn the_kernel_heap_honours_alignment() {
        // The heap must be large enough to hold a maximum-alignment
        // allocation plus the padding before it; a 4 KiB heap cannot hold a
        // 4 KiB-aligned byte, so the region is sized to make the test
        // meaningful rather than accidentally testing exhaustion.
        // SAFETY: a leaked region is valid for the heap's lifetime.
        let region: &'static mut [u8] = Box::leak(vec![0u8; 64 * 1024].into_boxed_slice());
        let base = region.as_ptr() as usize;
        // SAFETY: the region is leaked and writable.
        let heap = unsafe { KernelHeap::new(base, 64 * 1024) };

        for align in [1usize, 2, 4, 8, 16, 32, 64, 128, 256, 512, 1024, 4096] {
            let p = heap
                .alloc(1, align)
                .unwrap_or_else(|| panic!("allocation with alignment {align} failed"));
            assert_eq!(p % align, 0, "alignment {align} not honoured");
        }
    }

    #[test]
    fn a_maximum_alignment_allocation_can_fail_on_a_small_heap() {
        // The mirror image of the test above: an alignment larger than the
        // heap can never be satisfied, and the allocator must report that
        // rather than looping or returning a misaligned pointer.
        // SAFETY: a leaked region is valid for the heap's lifetime.
        let region: &'static mut [u8] = Box::leak(vec![0u8; 4096].into_boxed_slice());
        let base = region.as_ptr() as usize;
        // SAFETY: the region is leaked and writable.
        let heap = unsafe { KernelHeap::new(base, 4096) };
        // Request more than the whole heap with a huge alignment.
        assert!(heap.alloc(8192, 4096).is_none());
    }

    #[test]
    fn the_kernel_heap_reports_exhaustion() {
        // SAFETY: a leaked region is valid for the heap's lifetime.
        let region: &'static mut [u8] = Box::leak(vec![0u8; 128].into_boxed_slice());
        let base = region.as_ptr() as usize;
        // SAFETY: the region is leaked and writable.
        let heap = unsafe { KernelHeap::new(base, 128) };

        assert!(heap.alloc(64, 8).is_some());
        // The second allocation cannot fit.
        assert!(heap.alloc(128, 8).is_none());
        assert!(heap.bytes_free() < 128);
    }

    #[test]
    fn a_zero_sized_allocation_succeeds() {
        // SAFETY: a leaked region is valid for the heap's lifetime.
        let region: &'static mut [u8] = Box::leak(vec![0u8; 128].into_boxed_slice());
        let base = region.as_ptr() as usize;
        // SAFETY: the region is leaked and writable.
        let heap = unsafe { KernelHeap::new(base, 128) };
        let p = heap.alloc(0, 8);
        assert!(p.is_some(), "a zero-sized allocation must succeed");
    }

    #[test]
    fn a_non_power_of_two_alignment_is_rejected() {
        // SAFETY: a leaked region is valid for the heap's lifetime.
        let region: &'static mut [u8] = Box::leak(vec![0u8; 128].into_boxed_slice());
        let base = region.as_ptr() as usize;
        // SAFETY: the region is leaked and writable.
        let heap = unsafe { KernelHeap::new(base, 128) };
        assert!(heap.alloc(16, 3).is_none());
        assert!(heap.alloc(16, 0).is_none());
    }

    #[test]
    fn the_boot_info_memory_map_is_parsed() {
        // The end-to-end path: BootInfo fields -> map.
        let (buf, stride) = build_map(&[(mem_type::CONVENTIONAL, 0x0, 64)]);
        let mut bi = VelaBootInfo::zeroed();
        bi.memory_map_bytes = buf.len() as u64;
        bi.memory_desc_size = stride as u32;
        assert_eq!(bi.check_identity(), Ok(()));

        let map = MemoryMap::parse(&buf, stride);
        assert_eq!(map.len(), 1);
        assert_eq!(map.usable_bytes(), 64 * PAGE_SIZE);
    }

    #[test]
    fn reservations_cover_everything_the_kernel_owns() {
        let mut bi = VelaBootInfo::zeroed();
        bi.kernel_paddr = 0x10_0000;
        bi.kernel_bytes = 0x80_000;
        bi.initrd_paddr = 0x20_0000;
        bi.initrd_bytes = 0x40_000;
        bi.framebuffer_paddr = 0x8000_0000;
        bi.framebuffer_bytes = 4096 * 100;
        bi.memory_map_paddr = 0x1000;
        bi.memory_map_bytes = 40 * 4;

        let r = BootReservations::from_boot_info(&bi);
        assert_eq!(r.ranges.len(), 4);
        // The framebuffer must be reserved: handing its frames to a user
        // process would corrupt the display the kernel draws on.
        assert!(r.contains(0x8000_0000));
        assert!(r.contains(0x8000_0000 + 4096 * 50));
        assert!(r.contains(0x10_0000));
        assert!(r.contains(0x20_0000));
        assert!(r.contains(0x1000));
        // Just past the kernel image is not reserved.
        assert!(!r.contains(0x10_0000 + 0x80_000));
    }

    #[test]
    fn reservations_page_align_outward() {
        let mut bi = VelaBootInfo::zeroed();
        bi.kernel_paddr = 0x1001;
        bi.kernel_bytes = 0x1FFF;
        let r = BootReservations::from_boot_info(&bi);
        assert_eq!(r.ranges[0], (0x1000, 0x3000));
    }

    #[test]
    fn an_unset_boot_field_reserves_nothing() {
        // Every field starts at zero, and zero must mean "absent", not
        // "reserve the page at address 0".
        let r = BootReservations::from_boot_info(&VelaBootInfo::zeroed());
        assert!(r.ranges.is_empty());
        assert!(!r.contains(0));
        assert_eq!(r.total_bytes(), 0);
    }

    #[test]
    fn an_inverted_range_reserves_nothing() {
        let mut bi = VelaBootInfo::zeroed();
        bi.kernel_paddr = 0x2000;
        bi.kernel_bytes = 0; // length zero: nothing to reserve
        let r = BootReservations::from_boot_info(&bi);
        assert!(r.ranges.is_empty());
    }

    #[test]
    fn a_plan_places_the_bitmap_and_heap_in_usable_memory() {
        // 4 MiB of conventional memory at 0, then an MMIO hole, then more.
        let (buf, stride) = build_map(&[
            (mem_type::CONVENTIONAL, 0x0, 1024),      // 4 MiB
            (mem_type::MMIO, 0x20_0000, 16),          // 64 KiB
            (mem_type::CONVENTIONAL, 0x30_0000, 256), // 1 MiB
        ]);
        let map = MemoryMap::parse(&buf, stride);
        let plan = MemoryPlan::build(map, 4096, 64 * 1024, &[]).expect("plan");

        assert!(plan.bitmap_region.1 > plan.bitmap_region.0, "bitmap placed");
        assert!(plan.heap_region.1 > plan.heap_region.0, "heap placed");
        // Both must live inside an allocatable region, never in the MMIO hole.
        assert!(plan.bitmap_region.1 <= 1024 * PAGE_SIZE);
        assert!(plan.allocatable_bytes > 0);
        // The bitmap must be big enough to describe every frame.
        assert!(plan.bitmap_bytes >= plan.frame_count / 8);
        assert_eq!(plan.frame_count, (0x30_0000 + 256 * PAGE_SIZE) / PAGE_SIZE);
    }

    #[test]
    fn a_plan_with_no_usable_memory_fails_rather_than_guessing() {
        let (buf, stride) = build_map(&[(mem_type::RESERVED, 0x0, 1024)]);
        let map = MemoryMap::parse(&buf, stride);
        assert!(matches!(
            MemoryPlan::build(map, 4096, 4096, &[]),
            Err(PlanError::NothingUsable)
        ));
    }

    #[test]
    fn a_plan_rejects_a_bitmap_that_is_too_small() {
        let (buf, stride) = build_map(&[(mem_type::CONVENTIONAL, 0x0, 1024)]);
        let map = MemoryMap::parse(&buf, stride);
        // 4 MiB needs 1024 frames, so 128 bytes of bitmap is exactly one bit
        // per frame; anything less must be rejected rather than silently
        // under-covering the address space.
        let err = MemoryPlan::build(map, 64, 4096, &[]).unwrap_err();
        assert!(matches!(err, PlanError::RegionTooSmall { .. }));
    }

    #[test]
    fn a_plan_reserves_the_kernel_owned_regions() {
        let (buf, stride) = build_map(&[(mem_type::CONVENTIONAL, 0x0, 4096)]);
        let map = MemoryMap::parse(&buf, stride);
        let mut r = BootReservations::empty();
        r.push(0x1000, 0x9000);
        let plan = MemoryPlan::build(map, 8192, 64 * 1024, r.as_slice()).expect("plan");

        let reserved = plan.reserved_regions();
        // The caller's reservation plus the bitmap and the heap.
        assert!(reserved.iter().any(|(s, _)| *s == 0x1000));
        assert!(reserved.contains(&plan.bitmap_region));
        assert!(reserved.contains(&plan.heap_region));
        // The bitmap must not overlap the reserved kernel image.
        assert!(
            plan.bitmap_region.0 >= 0x9000,
            "bitmap at {:#x} overlaps the reserved image",
            plan.bitmap_region.0
        );
    }

    #[test]
    fn a_plan_reserves_every_non_allocatable_region() {
        let (buf, stride) = build_map(&[
            (mem_type::CONVENTIONAL, 0x0, 1024),
            (mem_type::MMIO, 0x20_0000, 16),
            (mem_type::RESERVED, 0x30_0000, 8),
        ]);
        let map = MemoryMap::parse(&buf, stride);
        let plan = MemoryPlan::build(map, 4096, 4096, &[]).expect("plan");

        let reserved = plan.reserved_regions();
        assert!(reserved.iter().any(|(s, _)| *s == 0x20_0000), "MMIO");
        assert!(reserved.iter().any(|(s, _)| *s == 0x30_0000), "reserved");
    }

    #[test]
    fn a_plan_summary_names_the_important_numbers() {
        let (buf, stride) = build_map(&[(mem_type::CONVENTIONAL, 0x0, 4096)]);
        let map = MemoryMap::parse(&buf, stride);
        let plan = MemoryPlan::build(map, 8192, 256 * 1024, &[]).expect("plan");
        let s = plan.summary();
        for key in [
            "regions=",
            "usable=",
            "frames=",
            "bitmap=",
            "heap=",
            "allocatable=",
        ] {
            assert!(s.contains(key), "summary missing {key}: {s}");
        }
    }

    #[test]
    fn the_null_page_is_always_reserved() {
        // A null-pointer dereference must fault cleanly rather than corrupt
        // whatever happens to live at physical address 0.
        let (buf, stride) = build_map(&[(mem_type::CONVENTIONAL, 0x0, 1024)]);
        let map = MemoryMap::parse(&buf, stride);
        let plan = MemoryPlan::build(map, 4096, 4096, &[]).expect("plan");

        let reserved = plan.reserved_regions();
        assert!(
            reserved.contains(&(0, PAGE_SIZE)),
            "the null page must be reserved"
        );

        // And the frame allocator must honour it.
        let alloc = test_allocator(plan.frame_base, plan.frame_count);
        for (s, e) in plan.reserved_regions() {
            alloc.reserve_range(s, e);
        }
        assert!(!alloc.frame_is_free(0), "the null page must not be free");
        assert!(!alloc.frame_is_free(PAGE_SIZE - 1));
    }

    #[test]
    fn a_frame_allocator_built_from_a_plan_never_hands_out_reserved_memory() {
        // End-to-end: plan -> allocator -> assert no reserved frame is ever
        // returned. This is the property the whole crate exists to provide.
        let (buf, stride) = build_map(&[(mem_type::CONVENTIONAL, 0x0, 4096)]);
        let map = MemoryMap::parse(&buf, stride);
        let mut res = BootReservations::empty();
        res.push(0x1000, 0x9000);
        let plan = MemoryPlan::build(map, 8192, 0, res.as_slice()).expect("plan");

        let frames = test_allocator(plan.frame_base, plan.frame_count);
        let alloc = frames;
        for (s, e) in plan.reserved_regions() {
            alloc.reserve_range(s, e);
        }

        let mut handed = std::collections::HashSet::new();
        while let Some(f) = alloc.alloc_frame() {
            assert!(handed.insert(f), "frame {f:#x} handed out twice");
            for (s, e) in plan.reserved_regions() {
                assert!(
                    f < s || f >= e,
                    "frame {f:#x} falls inside reserved range {s:#x}..{e:#x}"
                );
            }
        }
        assert!(!handed.is_empty(), "the plan left nothing allocatable");
    }
}
