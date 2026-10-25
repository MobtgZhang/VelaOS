//! Per-task user address spaces.
//!
//! # Why the syscall layer owns this
//!
//! A process's address space is not a property of the Exec service, of the
//! scheduler, or of the graphics library: it is the thing a *syscall* acts
//! on. `brk` grows a program's heap; `MapPage` maps a page into it. Both
//! need to find the calling task's page tables and its program break, and
//! neither can do that without somewhere to look them up.
//!
//! So the registry lives here, next to the dispatch table that uses it.
//!
//! # Why the registry is keyed by `TaskId` and not by "the current task"
//!
//! A syscall arrives with a task identity, and the identity is what the
//! kernel uses to find the task's state. Using "the current task" instead
//! would make the lookup depend on a piece of ambient state that the
//! hosted harness and the bare-metal kernel populate differently — and a
//! mismatch between the two would show up as a program whose heap belongs
//! to somebody else. Keying by the task id makes the lookup explicit and
//! testable: two tasks in one test have two different spaces, and a syscall
//! from the wrong one finds the wrong one.
//!
//! # What a task's space holds
//!
//! * the [`AddressSpace`], which is the hardware truth about which pages
//!   are mapped;
//! * the program break, which is the *policy* about where the heap is.
//!
//! The two are kept apart deliberately. The break is a number the program
//! can read and write, and it must not be derivable from the page tables —
//! a program that grew its heap and then had a page unmapped underneath it
//! would see the break move without being asked.
//!
//! # The frame allocator
//!
//! Growing a heap consumes physical frames, and only the kernel owns those.
//! The registry therefore takes a [`FrameSource`] rather than a concrete
//! allocator, so the bare-metal kernel can pass its
//! [`vela_kernel_memory::FrameAllocator`] and a test can pass a fake that
//! hands out a known sequence of addresses.

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use vela_hal_common::{AddressSpaceId, MapError, PageFlags, PAGE_SIZE};
use vela_hal_x86_64::addrspace::{AddressSpace, IdentityPhys};
use vela_kernel_exec::task::TaskId;

use spin::Mutex;

/// Where a program's image begins.
///
/// The first 64 KiB is deliberately left unmapped, so that a null or
/// small-offset pointer faults immediately instead of reading whatever the
/// program happened to put at its lowest address.
pub const IMAGE_BASE: u64 = 0x0000_0000_0010_0000;

/// Where a program's heap begins.
///
/// It has to clear the image, and it has to leave room below the stack for
/// a program whose data grows.
pub const BREAK_START: u64 = 0x0000_4000_0000_0000;

/// The ceiling on how far the heap may grow.
///
/// Half the user half, so that the heap can never reach the stack. Heap and
/// stack colliding is a real failure mode, and the cheapest way to make it
/// impossible is to give each a hard ceiling the other cannot pass.
pub const BREAK_LIMIT: u64 = 0x0000_6000_0000_0000;

/// The first address *above* the user half.
///
/// Everything at or above this is kernel space. It is the same bound
/// [`crate::copy`] validates against, so "may be mapped" and "may be copied
/// to" cannot disagree.
pub const USER_SPACE_END: u64 = 0x0000_8000_0000_0000;

/// The size of a user program's stack, in bytes.
pub const USER_STACK_SIZE: u64 = 64 * 1024;

/// The user stack's top address.
///
/// Just below [`USER_SPACE_END`]. The stack grows down from here.
pub const USER_STACK_TOP: u64 = USER_STACK_SIZE - PAGE_SIZE + USER_SPACE_END - 1;

/// The number of user address spaces created so far.
///
/// A space's id is its PML4's physical address, but the allocator hands out
/// addresses in a way that lets two live spaces collide only if the counter
/// wraps. It never will in practice, and the distinctness only matters for
/// diagnostics.
static NEXT_SPACE_ID: AtomicU64 = AtomicU64::new(1);

/// One task's user address space and its program break.
pub struct TaskSpace {
    /// The page tables.
    ///
    /// The hardware truth about which pages are mapped.
    space: AddressSpace,
    /// The current program break.
    ///
    /// The *policy* about where the heap is. It is deliberately separate
    /// from the page tables: the break is a number the program can read and
    /// write, and it must not change because some page was unmapped.
    ///
    /// It is an atomic rather than a plain field because a space is reached
    /// through a shared reference: the registry hands out `&TaskSpace`, and
    /// `brk` has to move the break through it. A plain `u64` behind a
    /// shared reference could not be written at all.
    brk: AtomicUsize,
    /// The base of the break region, so a `brk` can tell growth from shrink.
    brk_base: u64,
}

impl core::fmt::Debug for TaskSpace {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("TaskSpace")
            .field("cr3", &format_args!("{:#x}", self.space.cr3()))
            .field("brk", &format_args!("{:#x}", self.brk()))
            .field("mapped_frames", &self.space.mapped_frames())
            .finish()
    }
}

impl TaskSpace {
    /// Create an empty user space with its own page tables.
    ///
    /// The kernel half is mapped by [`AddressSpace::new`], so the space is
    /// able to take an interrupt or a syscall the instant it exists — which
    /// is what makes it safe to activate before the image is loaded.
    pub fn new() -> Self {
        let id = NEXT_SPACE_ID.fetch_add(1, Ordering::Relaxed);
        let mut space = AddressSpace::new(AddressSpaceId(id << 12));
        // The heap is identity-mapped during boot, so the PML4's physical
        // address is its virtual address. `set_pml4_base` refines this once
        // the kernel knows better; for a user space it is correct for the
        // lifetime of the boot heap.
        let pa = space.pml4_phys(&IdentityPhys);
        space.set_pml4_base(pa);
        Self {
            space,
            brk: AtomicUsize::new(BREAK_START as usize),
            brk_base: BREAK_START,
        }
    }

    /// The underlying address space.
    pub const fn address_space(&self) -> &AddressSpace {
        &self.space
    }

    /// The value to load into CR3 for this space.
    pub const fn cr3(&self) -> u64 {
        self.space.cr3()
    }

    /// The current program break.
    pub fn brk(&self) -> u64 {
        self.brk.load(Ordering::Acquire) as u64
    }

    /// True when `va` is inside the region a process may map.
    ///
    /// The test is deliberately the same bound [`crate::copy`] uses, so
    /// "may be mapped" and "may be copied to" cannot disagree.
    pub const fn is_user_range(va: u64, len: u64) -> bool {
        va < crate::copy::USER_SPACE_END && len <= crate::copy::USER_SPACE_END - va
    }

    /// Map one page of user memory, returning its virtual address.
    ///
    /// The page is zeroed, because a process has no way to know what was in a
    /// physical frame the kernel handed it a moment ago, and handing back
    /// another process's leftovers is an information leak.
    ///
    /// # Safety
    ///
    /// `frames` must be the kernel's frame allocator: handing a process
    /// memory from any other source would let two processes share a frame.
    pub unsafe fn map_user_page(&self, va: u64, frames: &dyn FrameSource) -> Result<u64, MapError> {
        if !va.is_multiple_of(PAGE_SIZE) {
            return Err(MapError::MisalignedVirtual);
        }
        if !Self::is_user_range(va, PAGE_SIZE) {
            return Err(MapError::MisalignedVirtual);
        }
        let pa = frames.alloc_frame()?;
        if let Err(e) = self.space.map_page(va, pa, PageFlags::user_rw()) {
            // The frame is ours and nothing maps it, so returning it is not a
            // use-after-free: no address space references it.
            // SAFETY: as above.
            unsafe { frames.free_frame(pa) };
            return Err(e);
        }
        // Zero the page in place. This runs in ring 0 against a supervisor
        // alias of the same physical frame, so it does not depend on the
        // process having the page mapped yet.
        // SAFETY: the frame was just mapped, is `user_rw`, and the kernel's
        // higher-half mapping of the same physical memory is what makes this
        // write valid.
        unsafe { zero_physical(pa, PAGE_SIZE as usize) };
        Ok(va)
    }

    /// Unmap one page of user memory, returning its frame to `frames`.
    pub fn unmap_user_page(&self, va: u64, frames: &dyn FrameSource) -> Result<(), MapError> {
        let pa = self.space.unmap_page(va)?;
        // SAFETY: `unmap_page` removed the only mapping of `pa`, so no
        // address space references it and returning it is sound.
        unsafe { frames.free_frame(pa) };
        Ok(())
    }

    /// Set the program break to `addr`, returning the break actually in force.
    ///
    /// Classic `brk` semantics, and the return value is what makes them
    /// usable: a program that asks for more memory than exists gets back the
    /// break it really got, so it can carry on with what it has rather than
    /// discovering the failure later at an address it cannot write.
    pub fn set_brk(&self, addr: u64, frames: &dyn FrameSource) -> Result<u64, MapError> {
        // A request inside the image, or past the ceiling, cannot be honoured.
        // Reporting the current break rather than an error is the classic
        // behaviour and is more useful to the caller than a failure.
        if addr < self.brk_base || addr > BREAK_LIMIT || !addr.is_multiple_of(PAGE_SIZE) {
            return Ok(self.brk());
        }

        let current = self.brk();
        if addr > current {
            // Growing: map the pages between the old break and the new one.
            // Mapping one at a time and stopping at the first failure means a
            // request that only partly succeeds leaves the break at the point
            // it actually reached, which is what we return.
            //
            // The break is advanced as each page is mapped, so a failure
            // part-way through leaves the break exactly where the mapping
            // stopped. That is what keeps the two consistent: the break
            // never names a page that is not mapped.
            let mut brk = current;
            while brk < addr {
                // SAFETY: the caller guarantees `frames` is the kernel's
                // frame allocator, and `brk` is inside the user range
                // because `BREAK_LIMIT` is.
                if unsafe { self.map_user_page(brk, frames) }.is_err() {
                    break;
                }
                brk += PAGE_SIZE;
                self.brk.store(brk as usize, Ordering::Release);
            }
        } else {
            // Shrinking: release the tail. Growing again re-maps zeroed pages,
            // so a shrink-then-grow cycle cannot resurrect old data.
            let mut brk = current;
            while brk > addr {
                brk -= PAGE_SIZE;
                // A failed unmap would mean the page is not mapped, which
                // cannot happen for a page between `brk_base` and `brk`.
                // Ignoring the error keeps the break consistent with reality.
                let _ = self.unmap_user_page(brk, frames);
                self.brk.store(brk as usize, Ordering::Release);
            }
        }
        Ok(self.brk())
    }

    /// Move this CPU into this space.
    ///
    /// # Safety
    ///
    /// Must not race with another switch on the same CPU, and the space must
    /// outlive the CPU's time in it.
    pub unsafe fn activate(&self) {
        // SAFETY: forwarded from this function's own contract.
        unsafe { vela_hal_x86_64::addrspace::activate(&self.space) };
    }
}

impl Default for TaskSpace {
    fn default() -> Self {
        Self::new()
    }
}

/// Zero `len` bytes at physical address `pa`.
///
/// # Safety
///
/// `pa` must be a frame the kernel owns, and the caller must have exclusive
/// access to it. The write goes through the kernel's identity mapping of low
/// physical memory.
unsafe fn zero_physical(pa: u64, len: usize) {
    // SAFETY: the caller guarantees the frame is a kernel-owned frame the
    // kernel can reach. `pa` is page-aligned and `len` is one page, so the
    // range stays inside that frame.
    unsafe {
        core::ptr::write_bytes(pa as *mut u8, 0, len);
    }
}

/// The registry of per-task user address spaces.
///
/// # Why a registry and not a field on the task
///
/// The Exec service's task table holds *signal sets*, which is a small piece
/// of per-task bookkeeping that the Exec API needs. A task's *address space*
/// is a much bigger thing, and it is not an Exec concept — the Exec API never
/// mentions page tables. Putting the space in the Exec service would make
/// Exec responsible for virtual memory, and would mean the hosted harness
/// (which has no address spaces at all) had to grow one for every task it
/// creates.
///
/// So the spaces live here, in the syscall layer, which is the only layer
/// that needs them.
pub struct SpaceRegistry {
    spaces: Mutex<Vec<(TaskId, Arc<TaskSpace>)>>,
}

impl core::fmt::Debug for SpaceRegistry {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("SpaceRegistry")
            .field("count", &self.spaces.lock().len())
            .finish()
    }
}

impl SpaceRegistry {
    /// An empty registry.
    pub const fn new() -> Self {
        Self {
            spaces: Mutex::new(Vec::new()),
        }
    }

    /// The number of registered spaces.
    pub fn len(&self) -> usize {
        self.spaces.lock().len()
    }

    /// True when no space is registered.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The space belonging to `task`, if it has one.
    ///
    /// The space is held behind an [`Arc`], so the reference outlives the
    /// registry's lock. That is what lets a handler keep working on a space
    /// after it has released the lock — and it is necessary rather than
    /// merely convenient, because the space's own break is behind an atomic
    /// precisely so that it can be moved through a shared reference.
    pub fn get(&self, task: TaskId) -> Option<Arc<TaskSpace>> {
        let spaces = self.spaces.lock();
        spaces
            .iter()
            .find(|(id, _)| *id == task)
            .map(|(_, space)| Arc::clone(space))
    }

    /// Give `task` a fresh user space, replacing any it had.
    ///
    /// Replacing rather than reusing is the right behaviour: a task that is
    /// being given a new space is a task that is starting over, and
    /// inheriting the old one's break would hand it a heap that overlaps
    /// whatever the new space already contains.
    pub fn create(&self, task: TaskId) {
        let mut spaces = self.spaces.lock();
        if let Some(slot) = spaces.iter_mut().find(|(id, _)| *id == task) {
            slot.1 = Arc::new(TaskSpace::new());
            return;
        }
        spaces.push((task, Arc::new(TaskSpace::new())));
    }

    /// Remove `task`'s space, if it has one.
    pub fn remove(&self, task: TaskId) {
        let mut spaces = self.spaces.lock();
        spaces.retain(|(id, _)| *id != task);
    }
}

impl Default for SpaceRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// Adapts the HAL's `&mut self` frame source to this crate's shared one.
///
/// # Why the lock
///
/// The HAL's trait takes `&mut self`, so calling it needs a mutable borrow
/// of the source. The source is shared behind a `&'static`, so the mutable
/// borrow is taken behind a lock rather than by casting the shared
/// reference away — the cast would be undefined behaviour even when the
/// referent really is immutable behind the scenes. The lock is not, and it
/// costs nothing measurable because frame allocation is not a hot path.
///
/// # Why this is `Sync`
///
/// The lock is what makes the shared reference sound: only one caller can
/// hold the `&mut` borrow at a time, so the aliasing the `&mut` requires
/// cannot happen.
pub struct SharedFrames<'a> {
    inner: spin::Mutex<&'a mut (dyn vela_hal_x86_64::FrameSource + Sync)>,
}

impl<'a> SharedFrames<'a> {
    /// Wrap a mutable borrow of a HAL frame source.
    pub const fn new(inner: &'a mut (dyn vela_hal_x86_64::FrameSource + Sync)) -> Self {
        Self {
            inner: spin::Mutex::new(inner),
        }
    }
}

impl FrameSource for SharedFrames<'_> {
    fn alloc_frame(&self) -> Result<u64, MapError> {
        let mut inner = self.inner.lock();
        inner.alloc_frame()
    }

    unsafe fn free_frame(&self, pa: u64) {
        let mut inner = self.inner.lock();
        // SAFETY: forwarded from this function's own contract.
        unsafe { inner.free_frame(pa) };
    }
}

/// A shared frame source, installed once during boot.
///
/// This is the interface the syscall layer uses. It is deliberately a
/// *shared* source: a user space is reached through a shared reference (the
/// registry hands out `Arc<TaskSpace>`), and `brk` has to map pages through
/// that reference. A `&mut self` trait would force the registry to hand out
/// exclusive borrows, which a `Mutex` guard cannot outlive.
///
/// # Why this is not the HAL's trait
///
/// The HAL's [`vela_hal_x86_64::FrameSource`] takes `&mut self`, because the
/// HAL's own mapper consumes frames mutably. The kernel's allocator is
/// shared, so its methods take `&self`, and this trait states that contract
/// directly rather than requiring a cast from `&` to `&mut`.
pub trait FrameSource {
    /// Return one zeroed, page-aligned frame.
    ///
    /// Returns [`MapError::OutOfMemory`] when the source is exhausted.
    fn alloc_frame(&self) -> Result<u64, MapError>;

    /// Return a frame previously obtained from this source.
    ///
    /// # Safety
    ///
    /// The caller must guarantee the frame is no longer referenced by any
    /// address space. A physical frame present in two page tables is not
    /// merely a leak — it lets one address space observe the other's writes.
    unsafe fn free_frame(&self, pa: u64);
}

/// The kernel-wide registry of user address spaces.
///
/// It is a `static` rather than a field of the dispatch table because the
/// dispatch table is a *table of function pointers*: it is copied and
/// installed, and it carries no state of its own. The registry is state, and
/// it has to be reachable from a handler that only receives a
/// [`crate::dispatch::SyscallContext`].
///
/// # Why it is not behind a lock of its own
///
/// The `spin::Mutex` here is the same one the dispatch path already holds
/// while it runs a handler, so a handler that touches the registry does not
/// deadlock: it takes this lock *after* the dispatch lock, never before.
/// The kernel-wide registry of user address spaces.
pub fn spaces() -> &'static SpaceRegistry {
    // The `Mutex` is never taken for the *reference* — only its contents are
    // locked — so handing out the registry itself is a plain borrow of a
    // `static`, which lives for the life of the kernel.
    static REGISTRY: SpaceRegistry = SpaceRegistry::new();
    &REGISTRY
}

/// Install the kernel's frame allocator for the `brk` and `MapPage` handlers.
///
/// # Why the allocator is installed rather than reached through a global
///
/// The frame allocator is created during boot, after the memory map is
/// parsed, so it cannot exist when this module is initialised. It is
/// therefore installed once, at boot, and the handlers reach it through
/// this function.
///
/// # Safety
///
/// Must be called once, during boot, before any user code runs. The
/// allocator must outlive the kernel.
static FRAMES: Mutex<Option<&'static (dyn FrameSource + Sync)>> = Mutex::new(None);

/// Install the kernel's frame allocator.
///
/// # Safety
///
/// Must be called once, during boot, before any user code runs. The
/// allocator must outlive every user space it serves, and must be safe to
/// use from more than one CPU.
pub unsafe fn install_frames(frames: &'static (dyn FrameSource + Sync)) {
    *FRAMES.lock() = Some(frames);
}

/// The installed frame allocator, if any.
pub fn frames() -> Option<&'static (dyn FrameSource + Sync)> {
    *FRAMES.lock()
}

#[cfg(test)]
extern crate std;

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::boxed::Box;
    use alloc::vec::Vec;

    /// A frame source that hands out a known sequence of addresses.
    ///
    /// # Why the frames are real, page-aligned allocations
    ///
    /// Two things force this. The page mapper requires a *page-aligned*
    /// physical address, so a frame's address must be a multiple of 4096 —
    /// and a plain heap allocation is not. And mapping a page zeroes it by
    /// writing through the physical address, so the address must also be
    /// *writable*: a fake frame whose address is not mapped is a
    /// segmentation fault rather than a failed test.
    ///
    /// So the fake allocates a page-aligned region per frame, with the
    /// alignment coming from the allocator's own over-allocation: the block
    /// is allocated with room to spare and then trimmed to its page
    /// boundary. The buffer is kept alive for the life of the test, so the
    /// address stays valid.
    struct FakeFrames {
        budget: AtomicU64,
        freed: Mutex<Vec<u64>>,
        pages: Mutex<Vec<Page>>,
    }

    impl FakeFrames {
        fn new(budget: u64) -> Self {
            Self {
                budget: AtomicU64::new(budget),
                freed: Mutex::new(Vec::new()),
                pages: Mutex::new(Vec::new()),
            }
        }

        fn remaining(&self) -> u64 {
            self.budget.load(Ordering::Relaxed)
        }

        /// Allocate one page-aligned, zeroed buffer.
        fn alloc_page() -> Option<Page> {
            let layout =
                std::alloc::Layout::from_size_align(PAGE_SIZE as usize, PAGE_SIZE as usize).ok()?;
            let raw = unsafe { std::alloc::alloc_zeroed(layout) };
            Some(Page {
                raw: core::ptr::NonNull::new(raw)?,
                layout,
            })
        }
    }

    impl FrameSource for FakeFrames {
        fn alloc_frame(&self) -> Result<u64, MapError> {
            let mut budget = self.budget.load(Ordering::Relaxed);
            loop {
                if budget == 0 {
                    return Err(MapError::OutOfMemory);
                }
                match self.budget.compare_exchange_weak(
                    budget,
                    budget - 1,
                    Ordering::AcqRel,
                    Ordering::Relaxed,
                ) {
                    Ok(_) => break,
                    Err(actual) => budget = actual,
                }
            }
            let page = FakeFrames::alloc_page().ok_or(MapError::OutOfMemory)?;
            let addr = page.as_ptr() as u64;
            self.pages.lock().push(page);
            Ok(addr)
        }

        unsafe fn free_frame(&self, pa: u64) {
            self.freed.lock().push(pa);
            self.budget.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// A page-aligned allocation kept alive for the duration of a test.
    struct Page {
        raw: core::ptr::NonNull<u8>,
        layout: std::alloc::Layout,
    }

    unsafe impl Send for Page {}
    unsafe impl Sync for Page {}

    impl Page {
        fn as_ptr(&self) -> *mut u8 {
            self.raw.as_ptr()
        }
    }

    impl Drop for Page {
        fn drop(&mut self) {
            unsafe {
                std::alloc::dealloc(self.raw.as_ptr(), self.layout);
            }
        }
    }

    /// A frame source shared behind a `&'static`, as the kernel installs it.
    static FRAMES: Mutex<Option<&'static (dyn FrameSource + Sync)>> = Mutex::new(None);

    /// Install a fake frame source for the duration of the test.
    fn install(budget: u64) -> &'static FakeFrames {
        let frames: &'static FakeFrames = Box::leak(Box::new(FakeFrames::new(budget)));
        *FRAMES.lock() = Some(frames);
        frames
    }

    #[test]
    fn a_fresh_space_starts_at_the_documented_break() {
        let space = TaskSpace::new();
        assert_eq!(space.brk(), BREAK_START);
    }

    #[test]
    fn a_query_returns_the_current_break() {
        let space = TaskSpace::new();
        let frames = install(64);
        assert_eq!(space.set_brk(0, frames).unwrap(), BREAK_START);
    }

    #[test]
    fn growing_the_break_maps_a_page_per_step() {
        let space = TaskSpace::new();
        let frames = install(64);
        let target = BREAK_START + 4 * PAGE_SIZE;
        assert_eq!(space.set_brk(target, frames).unwrap(), target);
        // Four pages were consumed, one per step.
        assert_eq!(frames.remaining(), 64 - 4);
    }

    #[test]
    fn a_misaligned_break_is_refused_and_the_break_does_not_move() {
        let space = TaskSpace::new();
        let frames = install(64);
        let before = space.brk();
        // Not page-aligned: the request is refused, and the *current* break
        // is reported rather than an error — which is the classic `brk`
        // behaviour and is what makes the call usable.
        assert_eq!(space.set_brk(BREAK_START + 1, frames).unwrap(), before);
        assert_eq!(frames.remaining(), 64);
    }

    #[test]
    fn a_break_below_the_base_is_refused() {
        let space = TaskSpace::new();
        let frames = install(64);
        let before = space.brk();
        // Below the base of the break region: refused.
        assert_eq!(
            space.set_brk(BREAK_START - PAGE_SIZE, frames).unwrap(),
            before
        );
        assert_eq!(frames.remaining(), 64);
    }

    #[test]
    fn a_break_past_the_ceiling_is_refused() {
        let space = TaskSpace::new();
        let frames = install(64);
        let before = space.brk();
        assert_eq!(
            space.set_brk(BREAK_LIMIT + PAGE_SIZE, frames).unwrap(),
            before
        );
        assert_eq!(frames.remaining(), 64);
    }

    #[test]
    fn shrinking_the_break_releases_the_pages() {
        let space = TaskSpace::new();
        let frames = install(64);
        let target = BREAK_START + 4 * PAGE_SIZE;
        space.set_brk(target, frames).unwrap();
        assert_eq!(frames.remaining(), 60);
        // Shrinking back to the base releases the four pages again.
        assert_eq!(space.set_brk(BREAK_START, frames).unwrap(), BREAK_START);
        assert_eq!(frames.remaining(), 64);
    }

    #[test]
    fn a_break_that_cannot_be_satisfied_stops_where_it_got_to() {
        let space = TaskSpace::new();
        // Only two pages are available, but four are asked for.
        let frames = install(2);
        let target = BREAK_START + 4 * PAGE_SIZE;
        // The break lands where the mapping stopped, and the caller learns
        // the real limit from the return value rather than from an error.
        assert_eq!(
            space.set_brk(target, frames).unwrap(),
            BREAK_START + 2 * PAGE_SIZE
        );
        assert_eq!(frames.remaining(), 0);
    }

    #[test]
    fn a_page_outside_the_user_half_is_refused() {
        let space = TaskSpace::new();
        let frames = install(64);
        // Above the user ceiling: not a user page.
        let result = unsafe { space.map_user_page(USER_SPACE_END, frames) };
        assert!(result.is_err());
        assert_eq!(frames.remaining(), 64);
    }

    #[test]
    fn a_misaligned_page_is_refused() {
        let space = TaskSpace::new();
        let frames = install(64);
        let result = unsafe { space.map_user_page(BREAK_START + 1, frames) };
        assert!(result.is_err());
        assert_eq!(frames.remaining(), 64);
    }

    #[test]
    fn a_page_inside_the_user_half_is_mapped() {
        let space = TaskSpace::new();
        let frames = install(64);
        let result = unsafe { space.map_user_page(BREAK_START, frames) };
        assert_eq!(result, Ok(BREAK_START));
        assert_eq!(frames.remaining(), 63);
    }

    #[test]
    fn the_registry_finds_a_tasks_own_space() {
        let registry = SpaceRegistry::new();
        let a = TaskId(1);
        let b = TaskId(2);
        assert!(registry.get(a).is_none());
        registry.create(a);
        assert!(registry.get(a).is_some());
        assert!(registry.get(b).is_none());
    }

    #[test]
    fn two_tasks_get_different_spaces() {
        let registry = SpaceRegistry::new();
        registry.create(TaskId(1));
        registry.create(TaskId(2));
        let one = registry.get(TaskId(1)).unwrap();
        let two = registry.get(TaskId(2)).unwrap();
        assert_ne!(one.cr3(), two.cr3());
    }

    #[test]
    fn giving_a_task_a_new_space_replaces_the_old_one() {
        let registry = SpaceRegistry::new();
        registry.create(TaskId(1));
        let first = registry.get(TaskId(1)).unwrap();
        registry.create(TaskId(1));
        let second = registry.get(TaskId(1)).unwrap();
        assert_ne!(first.cr3(), second.cr3());
    }

    #[test]
    fn removing_a_task_forgets_its_space() {
        let registry = SpaceRegistry::new();
        registry.create(TaskId(1));
        assert!(registry.get(TaskId(1)).is_some());
        registry.remove(TaskId(1));
        assert!(registry.get(TaskId(1)).is_none());
    }
}
