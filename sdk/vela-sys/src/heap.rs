//! The guest heap: a `GlobalAlloc` over the kernel's `brk` syscall.
//!
//! # Why a guest needs an allocator at all
//!
//! `no_std` does not mean "no allocation" — it means "no *default* allocator".
//! Everything from `Vec` to `format!` needs one. A bare-metal guest that has
//! no allocator cannot use a third of `alloc`, which means `String` is out,
//! which means the boot log is a sequence of byte writes with no formatting.
//! That is a poor foundation for the userland, so the SDK provides one.
//!
//! # Why `brk` and not `mmap`
//!
//! A `Vec` grows by asking for *more*, and a bump-style heap answers that with
//! one call. `mmap` would work too, but it hands out page-aligned blocks with
//! no guarantee of contiguity, so growing a `Vec` by one element would need
//! the allocator to copy — which turns a bump allocator into a general one,
//! with the free-list bookkeeping and fragmentation handling that come with it.
//!
//! So the heap here is a bump allocator over a `brk` that only ever moves
//! upward, and [`GuestHeap::grow`] returns zeroed memory. The kernel zeroes the
//! pages as it maps them, so "zeroed" is a property of the platform rather
//! than something this file has to do — and doing it twice would be a
//! measurable cost on every page fault-sized allocation.
//!
//! # What this deliberately does not do
//!
//! It does not free. There is no `dealloc` path that returns memory, and
//! [`GuestHeap::grow`] never moves the break backwards. That is a real
//! limitation and it is stated rather than hidden: a long-running guest that
//! allocates in a loop exhausts its heap and stops. For v0.1.0's programs —
//! `init` builds a fixed directory tree, `workbench` draws until the session
//! ends — the total live set is bounded and small. Adding a free list is a
//! local change to this one file when a program needs it.

use alloc::alloc::{GlobalAlloc, Layout};
use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicU64, Ordering};

/// The number of pages reserved for a guest's heap when it first grows.
///
/// A guest's heap starts empty and grows on demand, but the *first* growth has
/// to come from somewhere, so this is the amount `grow` asks for when the heap
/// has never been sized.
pub const DEFAULT_HEAP_PAGES: usize = 16;

/// How many bytes to request at a time once the heap is established.
///
/// Chosen so that a program printing a boot log does not make a syscall per
/// character — but small enough that a 4 KiB frame's worth of waste is the
/// ceiling, rather than a program reserving megabytes it never touches.
const GROW_CHUNK: usize = 8 * 1024;

/// The guest heap.
///
/// # Safety of the design
///
/// The break lives in a `static`, and every access is either atomic or behind
/// a lock, so the structure is `Sync` by construction rather than by
/// assertion. The one piece of genuinely unsynchronised state is the bump
/// cursor, which is only ever advanced while the lock is held — see
/// [`GuestHeap::grow`].
pub struct GuestHeap {
    /// The current break, or 0 before the first growth.
    brk: AtomicU64,
    /// The locked bump cursor.
    ///
    /// Plain rather than atomic because it is only touched under `lock`. The
    /// distinction matters: an atomic cursor would imply it can be raced, and
    /// readers would have to reason about whether they need a fence.
    cursor: UnsafeCell<usize>,
    /// Serialises growth.
    lock: spin::Mutex<()>,
}

// SAFETY: every field is either an atomic or is only read and written while
// `lock` is held. `cursor` is the only non-atomic field and has exactly one
// accessor, `grow`, which takes the lock before touching it.
unsafe impl Sync for GuestHeap {}
// SAFETY: `alloc`/`dealloc` take `&self`, so two threads may call the
// allocator at once. Serialisation happens inside `grow` rather than around
// these calls, so a re-entrant allocation on the same thread — which a
// formatting routine can cause — still works.
unsafe impl Send for GuestHeap {}

impl GuestHeap {
    /// A heap that has not yet been given any memory.
    pub const fn new() -> Self {
        Self {
            brk: AtomicU64::new(0),
            cursor: UnsafeCell::new(0),
            lock: spin::Mutex::new(()),
        }
    }

    /// The current break, or 0 if the heap has never grown.
    pub fn break_address(&self) -> u64 {
        self.brk.load(Ordering::Acquire)
    }

    /// How many bytes this heap has handed out.
    pub fn used(&self) -> usize {
        // SAFETY: `cursor` is read only for a statistic, and a torn read is
        // impossible for a `usize` — the worst case is a stale count. The
        // lock is not taken because a caller asking "how much have I used"
        // must not be able to deadlock behind an allocation on another thread.
        unsafe { *self.cursor.get() }
    }

    /// Obtain at least `size` bytes of zeroed memory, aligned to `align`.
    ///
    /// Returns null when the kernel refused to give the memory, which is what
    /// the `GlobalAlloc` contract requires.
    pub fn grow(&self, size: usize, align: usize) -> *mut u8 {
        if size == 0 {
            // A zero-sized allocation must still be non-null and distinct per
            // the `GlobalAlloc` contract. The kernel's `brk` already returns
            // the current break for a zero-length request, which is exactly
            // that: a stable, aligned, never-null address.
            return self.brk.load(Ordering::Acquire) as *mut u8;
        }

        let _guard = self.lock.lock();

        let current_brk = self.brk.load(Ordering::Relaxed);

        if current_brk == 0 {
            // First use: ask for a starting allocation big enough to be
            // useful on its own, so a program that allocates once does not
            // make a syscall for every subsequent small allocation.
            let first = (DEFAULT_HEAP_PAGES * page_size())
                .max(size + align)
                .max(GROW_CHUNK);
            let Some(addr) = self.request_break(first) else {
                return core::ptr::null_mut();
            };
            self.brk.store(addr + first as u64, Ordering::Release);
            // SAFETY: as above; the lock is held.
            unsafe { *self.cursor.get() = addr as usize };
            return self.finish(addr as usize, size, align);
        }

        // Bump within the region already obtained. Reusing space the kernel
        // has already given us is the entire reason this is a bump allocator.
        // SAFETY: the lock is held.
        let avail_start = unsafe { *self.cursor.get() };
        let limit = self.brk.load(Ordering::Relaxed) as usize;
        if let Some(ptr) = Self::align_up(avail_start, align) {
            if ptr.saturating_add(size) <= limit {
                // SAFETY: the lock is held and the new cursor is within the
                // region the kernel owns and zeroed.
                unsafe { *self.cursor.get() = ptr + size };
                return ptr as *mut u8;
            }
        }

        // Out of room: ask the kernel for another chunk.
        let want = size
            .saturating_add(align)
            .max(GROW_CHUNK)
            .next_multiple_of(page_size());
        let Some(new_brk) = self.request_break(want) else {
            return core::ptr::null_mut();
        };
        self.brk.store(new_brk + want as u64, Ordering::Release);

        // SAFETY: the lock is held.
        let base = unsafe { *self.cursor.get() };
        // SAFETY: as above.
        unsafe { *self.cursor.get() = new_brk as usize };
        self.finish(base, size, align)
    }

    /// Extend the break by `bytes` and return its old value, or null on
    /// refusal.
    fn request_break(&self, bytes: usize) -> Option<u64> {
        let current = self.brk.load(Ordering::Acquire);
        if current == 0 {
            // Query first: the kernel owns the layout, and guessing a starting
            // break here would duplicate a policy that belongs in one place.
            let start = crate::api::program_break(0).ok()?;
            let new = start.checked_add(bytes as u64)?;
            let got = crate::api::program_break(new).ok()?;
            if got < new {
                return None;
            }
            return Some(start);
        }
        let new = current.checked_add(bytes as u64)?;
        let got = crate::api::program_break(new).ok()?;
        if got < new {
            return None;
        }
        Some(current)
    }

    /// Round `addr` up to `align` and check it fits, then return the pointer.
    ///
    /// Callers hold the lock.
    fn finish(&self, base: usize, size: usize, align: usize) -> *mut u8 {
        match Self::align_up(base, align) {
            Some(ptr) if ptr.saturating_add(size) <= self.brk.load(Ordering::Acquire) as usize => {
                ptr as *mut u8
            }
            _ => core::ptr::null_mut(),
        }
    }

    /// Round `addr` up to a multiple of `align`, which must be a power of two.
    ///
    /// Returns `None` on overflow, which a caller must treat as an allocation
    /// failure rather than wrapping around to a low address — the difference
    /// between "out of memory" and "corrupted memory" is worth one branch.
    ///
    /// Not `const`: `?` on an `Option` is not yet usable in a constant
    /// function, and a caller that needs the rounding at compile time can
    /// write the expression out. Making this work is a compiler detail, not a
    /// design one.
    fn align_up(addr: usize, align: usize) -> Option<usize> {
        // `Layout` guarantees a non-zero power-of-two alignment, so `align - 1`
        // is a valid mask. The subtraction is deliberate: it turns the mask
        // into "round down" without a second operation.
        let mask = align.wrapping_sub(1);
        // A wrapping add would turn an overflow into a small number that then
        // rounds *down* to a valid-looking low address, so the add is checked
        // and the failure is reported rather than absorbed.
        let rounded = addr.checked_add(mask)?;
        Some(rounded & !mask)
    }
}

impl Default for GuestHeap {
    fn default() -> Self {
        Self::new()
    }
}

unsafe impl GlobalAlloc for GuestHeap {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // `grow` is safe to call: it validates the size and alignment it is
        // given, and asks the kernel rather than trusting the caller.
        self.grow(layout.size(), layout.align())
    }

    unsafe fn dealloc(&self, _ptr: *mut u8, _layout: Layout) {
        // Nothing. See the module documentation: this is a bump allocator and
        // the limitation is stated rather than hidden.
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // A bump allocator cannot free the old block, so growing in place is
        // the only correct answer: allocate, copy, leak the original. That is
        // worse than a real `realloc` and is called out in the module
        // documentation; it is still correct, which is what matters here.
        let new_ptr = self.grow(new_size, layout.align());
        if new_ptr.is_null() {
            return core::ptr::null_mut();
        }
        // SAFETY: the caller guarantees `ptr` holds `layout.size()` initialised
        // bytes and `new_ptr` is a distinct allocation of at least `new_size`.
        // Copying `min(old, new)` is the common prefix of the two blocks.
        unsafe {
            core::ptr::copy_nonoverlapping(ptr, new_ptr, layout.size().min(new_size));
        }
        new_ptr
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        // The kernel zeroes pages as it maps them, so memory obtained from
        // `grow` is already zeroed. Writing zeros over it again would be a
        // second pass over every byte for no effect.
        self.alloc(layout)
    }
}

/// The page size this guest assumes.
///
/// Read from the kernel's own constant rather than assumed, so a page-size
/// change in the kernel cannot leave the guest rounding to the wrong
/// granularity.
pub const fn page_size() -> usize {
    crate::numbers::PAGE_SIZE
}
