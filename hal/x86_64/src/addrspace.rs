//! Ownership and manipulation of page tables.
//!
//! [`AddressSpace`] is the only type in VelaOS that may write a page-table
//! entry. Requiring `&mut AddressSpace` for every write is what turns a
//! double-free or use-after-free of a mapping into a compile error rather than
//! a runtime hazard.
//!
//! # Why the tables live in the heap
//!
//! The obvious implementation keeps page-table frames at *physical* addresses
//! and reaches them through an identity mapping. That is how most boot code
//! does it, but it has two problems for VelaOS:
//!
//! 1. It makes the whole address-space implementation untestable on the host,
//!    because every table access dereferences memory that only exists on the
//!    target. Phase 3 needs paging verified by unit tests, not only by booting.
//! 2. It needs an allocator before the heap exists, which is a chicken-and-
//    egg problem at exactly the moment the kernel is trying to build the heap.
//!
//! So the table *tree* here is ordinary heap memory: each level is a
//! `Box<[PageTableEntry; 512]>`. The kernel heap (Phase 3) is itself built on a
//! bump region reserved before `ExitBootServices`, so this is available from
//! the first instruction. Physical addresses are needed only at two moments,
//! both explicit:
//!
//! * when the boot space is created, to hand its PML4 base to the firmware's
//!   already-loaded CR3, and
//! * when [`AddressSpace::activate`] loads CR3.
//!
//! Both go through [`AddressSpace::phys_of`], supplied by a
//! [`PhysOfVirt`] implementation. Mapping *data* still takes real physical
//! frames from a [`FrameSource`], and `unmap` hands the frame back, so frame
//! accounting stays exact.

use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicUsize, Ordering};

use spin::Mutex;

use vela_hal_common::{AddressSpaceId, MapError, PageFlags, PAGE_SIZE};

use crate::paging::{
    bytes_covered, encode_flags, entry_flags, flush, invlpg, PageLevel, PageTableEntry,
    ENTRIES_PER_TABLE, INTERMEDIATE_LEVELS, KERNEL_VIRTUAL_BASE,
};
use crate::regs;

/// One page-table level, heap-allocated.
///
/// # Why the alignment is load-bearing
///
/// A page table's **physical** address has its low 12 bits used as the
/// *flag* bits of the entry that points at it. A table that is not
/// page-aligned therefore has an address whose low bits are read by the CPU
/// as flags — the present bit may be clear, the writable bit may be set, and
/// the address bits themselves are wrong. The result is a walk that either
/// faults immediately or, worse, follows a garbage pointer.
///
/// `Box` honours the type's alignment, so an over-aligned array gives a
/// page-aligned table for free. The alternative — a plain
/// `Box<[PageTableEntry; 512]>` — is 4 KiB but only 8-byte aligned, which is
/// exactly the bug this type exists to prevent.
#[repr(C, align(4096))]
struct TableEntries([PageTableEntry; ENTRIES_PER_TABLE]);

/// One page-table level, heap-allocated.
type Table = Box<TableEntries>;

/// How to translate a heap pointer into the physical address the CPU needs in
/// CR3.
///
/// During boot the kernel heap is identity-mapped, so the identity
/// implementation is correct and is what `kernel/entry` installs. A future
/// higher-half heap supplies a real translation.
pub trait PhysOfVirt {
    /// Physical address backing `virt`.
    fn phys_of(&self, virt: *const u8) -> u64;
}

/// The identity translation, valid while the heap is identity-mapped.
#[derive(Debug, Clone, Copy, Default)]
pub struct IdentityPhys;

impl PhysOfVirt for IdentityPhys {
    fn phys_of(&self, virt: *const u8) -> u64 {
        virt as u64
    }
}

/// The identity translation as a bare function pointer.
///
/// The [`AddressSpace`] stores its translation as a function pointer rather
/// than a trait object so that the space stays `Sized` and the tree walk has
/// no vtable indirection on a hot path.
fn identity_phys(virt: *const u8) -> u64 {
    virt as u64
}

/// Round `value` up to the next multiple of `align`.
///
/// `align` must be a power of two, which every caller here guarantees: the
/// only alignment used is the 2 MiB large-page size.
fn align_up_to(value: u64, align: u64) -> u64 {
    (value + align - 1) & !(align - 1)
}

/// A source of physical frames for *data* mappings.
///
/// Distinct from table allocation on purpose: tables come from the heap, and
/// only data pages are handed out from the physical memory manager.
pub trait FrameSource {
    /// Return one zeroed, page-aligned frame.
    fn alloc_frame(&mut self) -> Result<u64, MapError>;
    /// Return a frame previously obtained from this source.
    ///
    /// # Safety
    ///
    /// The caller must guarantee the frame is no longer referenced by any
    /// address space. A physical frame present in two PTEAs is not merely a
    /// leak — it lets one address space observe the other's writes.
    unsafe fn free_frame(&mut self, pa: u64);
}

/// A node of the table tree.
///
/// `children` mirrors the hardware exactly: one optional lower table per entry
/// index. Modelling it as 512 slots rather than a single `Option<Box<Node>>` is
/// not a stylistic choice — the index is what selects the child, so a single
/// child would silently alias every path through this level onto one subtree.
struct Node {
    table: Table,
    /// Lower table reachable through each entry, or `None` if that entry has
    /// never been a pointer.
    ///
    /// `Option<Box<Node>>` is niche-optimised to a plain pointer, so this costs
    /// exactly the same 4 KiB as the entry array it parallels.
    children: [Option<Box<Node>>; ENTRIES_PER_TABLE],
}

impl Node {
    fn new() -> Self {
        Self {
            // The over-aligned `TableEntries` gives a page-aligned table.
            table: Box::new(TableEntries([PageTableEntry::empty(); ENTRIES_PER_TABLE])),
            // `Box<Node>` is non-null, so `None` needs no space of its own.
            children: [const { None }; ENTRIES_PER_TABLE],
        }
    }

    /// Read entry `index`.
    fn read(&self, index: usize) -> PageTableEntry {
        debug_assert!(index < ENTRIES_PER_TABLE);
        // SAFETY: `index` is a 9-bit field taken from a virtual address, so it
        // is always below `ENTRIES_PER_TABLE`; the debug assert above states
        // that invariant for debug builds.
        unsafe { *self.table.0.get_unchecked(index) }
    }

    /// Write entry `index`.
    fn write(&mut self, index: usize, entry: PageTableEntry) {
        debug_assert!(index < ENTRIES_PER_TABLE);
        // SAFETY: as in `read`.
        unsafe {
            *self.table.0.get_unchecked_mut(index) = entry;
        }
    }
}

/// A whole address space: a PML4 and the tables reachable from it.
pub struct AddressSpace {
    /// Identity of this space; the physical base of its PML4.
    id: AddressSpaceId,
    root: Node,
    /// Leaf mappings created through [`Self::map_range`].
    ///
    /// An atomic rather than a plain counter because every mutating method
    /// takes `&self` — the lock inside serialises the *tree*, but a caller
    /// holding only `&AddressSpace` still has to be able to bump the count.
    mapped_frames: AtomicUsize,
    /// Serialises tree mutation.
    ///
    /// Held only across in-memory pointer walks — no physical allocation
    /// happens under it — so the critical section is a few dozen instructions.
    lock: Mutex<()>,
    /// How to turn a table's heap address into the physical address the CPU
    /// needs in a page-table entry.
    ///
    /// The identity is correct for VelaOS, and not only during boot: the
    /// kernel's heap is mapped at the *same* virtual and physical address in
    /// the kernel's own tables, so every table the kernel builds lives at an
    /// address whose physical counterpart is its own. The translation is
    /// therefore the identity for the whole life of the kernel, and the field
    /// exists so that the assumption is *stated* in one place rather than
    /// spread across the tree-walking code.
    phys_of: fn(*const u8) -> u64,
}

impl core::fmt::Debug for AddressSpace {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("AddressSpace")
            .field("id", &self.id)
            .field("cr3", &format_args!("{:#x}", self.id.0))
            .field("mapped_frames", &self.mapped_frames)
            .finish()
    }
}

impl AddressSpace {
    /// Create an empty address space.
    ///
    /// The only pre-populated path is the kernel's higher half: every address
    /// space must be able to reach kernel code, and building that path here
    /// means a task created in Phase 4 is valid the instant it is switched to.
    /// The low half starts empty and belongs to userspace.
    pub fn new(id: AddressSpaceId) -> Self {
        let space = Self {
            id,
            root: Node::new(),
            mapped_frames: AtomicUsize::new(0),
            lock: Mutex::new(()),
            phys_of: identity_phys,
        };
        space.ensure_path(KERNEL_VIRTUAL_BASE);
        space
    }

    /// This space's identity, which is also the physical base of its PML4 once
    /// [`Self::install`] has recorded it.
    pub const fn id(&self) -> AddressSpaceId {
        self.id
    }

    /// The value to load into CR3.
    pub const fn cr3(&self) -> u64 {
        self.id.0
    }

    /// Re-point this space at a different PML4 base.
    ///
    /// Called once by `kernel/entry` after the PML4's heap allocation has been
    /// given a physical address. The identity constructor cannot know that
    /// address, so `new` seeds the id from the allocator and `install` refines
    /// it.
    pub fn set_pml4_base(&mut self, pml4_pa: u64) {
        self.id = AddressSpaceId(pml4_pa);
    }

    /// Leaf mappings currently present.
    pub fn mapped_frames(&self) -> usize {
        // Relaxed is enough: this is a statistic, and the lock that guards the
        // tree is what makes the mapping itself correct.
        self.mapped_frames.load(Ordering::Relaxed)
    }

    /// True when `va` has a mapping, without decoding the physical address.
    pub fn is_mapped(&self, va: u64) -> bool {
        self.translate(va).is_some()
    }

    /// Create every table on the path to `va`. Caller holds the lock.
    fn ensure_path(&self, va: u64) {
        let _guard = self.lock.lock();
        self.ensure_path_locked(va, INTERMEDIATE_LEVELS.len());
    }

    /// Create the tables on the path to `va`, down to `depth` levels.
    fn ensure_path_to_depth(&self, va: u64, depth: usize) {
        let _guard = self.lock.lock();
        self.ensure_path_locked(va, depth);
    }

    /// Create the tables on the path to `va`, down to `depth` levels.
    ///
    /// `depth` counts the levels *below* the root, so `1` creates only the
    /// PDPT, `2` creates the PDPT and the page directory, and `3` creates the
    /// whole path down to the leaf page table.
    ///
    /// The depth matters because of what a large page is. A 2 MiB leaf lives
    /// in the **page directory**, so a mapping that only needs that leaf has
    /// no use for the page table that would sit below it. Creating it anyway
    /// is not merely wasteful: it would put a *table pointer* into the very
    /// page-directory slot the large page belongs in, and the large page
    /// could then never be installed — the slot would look occupied.
    ///
    /// Creating the page table costs 8 KiB of heap per 2 MiB slot as well,
    /// which for a 512 MiB identity map is 2 MiB of heap spent on tables the
    /// hardware never reads.
    fn ensure_path_locked(&self, va: u64, depth: usize) {
        let mut node = &self.root as *const Node as *mut Node;
        // SAFETY: the lock is held for the whole walk, and every `children`
        // entry is created here under the same lock.
        unsafe {
            // The root *is* the PML4, so the walk visits three levels and
            // lands on the page table. See [`INTERMEDIATE_LEVELS`].
            for (level_number, level) in INTERMEDIATE_LEVELS.iter().enumerate() {
                if level_number >= depth {
                    break;
                }
                let index = level.index(va);
                if (*node).read(index).is_leaf() {
                    // A large page already covers this range. There is no
                    // table below it, so the walk stops here: descending would
                    // mean replacing the large page with a table pointer, and
                    // the caller must be told the range is already mapped.
                    break;
                }
                let missing = (*node).children[index].is_none();
                if missing {
                    let child = Box::new(Node::new());
                    // The child's physical address goes into *this* level's
                    // entry, and it has to be the child's real physical
                    // address: an entry with no address in it points nowhere,
                    // and the CPU's page-table walk would fault on the first
                    // access through the path.
                    //
                    // The translation is the identity because the kernel's
                    // heap is mapped at the same virtual and physical
                    // address; see the `phys_of` field.
                    //
                    // The address taken is that of the *entries*, not of the
                    // `TableEntries` wrapper. They are the same address —
                    // the wrapper is a `repr(C)` newtype around the array and
                    // is `align(4096)` — but naming the array makes the
                    // intent explicit: this value is what goes into a
                    // page-table entry, and it must be the table's address.
                    let child_pa = (self.phys_of)(child.table.0.as_ptr() as *const u8);
                    (*node).children[index] = Some(child);
                    // Intermediate entries are always supervisor-present and
                    // writable. Never `user`: a userspace walk must not be able
                    // to leave the kernel half even transiently.
                    //
                    // The `AVAILABLE` bit marks the entry as a table pointer
                    // rather than a leaf, which is how `find_leaf_locked`
                    // tells the two apart.
                    (*node).write(
                        index,
                        PageTableEntry(
                            child_pa
                                | entry_flags::PRESENT
                                | entry_flags::WRITABLE
                                | entry_flags::AVAILABLE,
                        ),
                    );
                }
                node = (*node).children[index].as_deref_mut().unwrap() as *mut Node;
            }
        }
    }

    /// Map one 2 MiB page of `pa` at `va`.
    ///
    /// # Why this exists
    ///
    /// The kernel's address space has to reach *every* physical frame: the
    /// frame allocator hands out physical addresses, `kernel/syscall`'s
    /// `zero_physical` writes through a physical address directly, and the
    /// loader's boot pool and the framebuffer are named by physical address.
    /// Mapping all of that with 4 KiB pages would need one page table per
    /// 4 KiB of RAM — 131 072 tables for a 512 MiB machine, which does not
    /// fit in the 4 MiB kernel heap.
    ///
    /// A 2 MiB page collapses 512 of those entries into one, so a 512 MiB
    /// identity map costs 256 tables instead of 131 072. The hardware
    /// requires `CR4.PSE` for this on a legacy CPU; on a long-mode CPU the
    /// bit must be *clear*, and it is — the long-mode paging control register
    /// (`CR4`) has no `PSE` bit, and the loader's own tables already use 2 MiB
    /// pages on the same machine.
    ///
    /// # Preconditions
    ///
    /// `va` and `pa` must both be 2 MiB-aligned. A large page maps a
    /// 2 MiB-aligned *physical* frame, and the hardware ignores the low
    /// 21 bits of the address in the entry, so an unaligned `pa` would map
    /// the wrong frame.
    ///
    /// # Errors
    ///
    /// [`MapError::MisalignedVirtual`] or [`MapError::MisalignedPhysical`] if
    /// either address is not 2 MiB-aligned, and [`MapError::AlreadyMapped`]
    /// if the slot already holds a *different* mapping.
    pub fn map_large_page(&self, va: u64, pa: u64, flags: PageFlags) -> Result<(), MapError> {
        const LARGE: u64 = 2 * 1024 * 1024;
        if !va.is_multiple_of(LARGE) {
            return Err(MapError::MisalignedVirtual);
        }
        if !pa.is_multiple_of(LARGE) {
            return Err(MapError::MisalignedPhysical);
        }
        self.ensure_path_to_depth(va, 2);
        let _guard = self.lock.lock();
        // The page *directory* is the level a 2 MiB leaf lives in, so the
        // walk stops one level above the leaf table. The root is the PML4,
        // so the walk covers the PML4 and the PDPT and lands on the page
        // directory.
        let node = self.find_directory_locked(va).ok_or(MapError::NotMapped)?;
        let index = PageLevel::PageDirectory.index(va);
        // SAFETY: the lock is held across this read-modify-write, and the
        // pointer came from a live tree node.
        unsafe {
            let existing = (*node).read(index);
            if existing.is_present() {
                if existing.physical() != pa {
                    return Err(MapError::AlreadyMapped);
                }
                (*node).write(index, PageTableEntry::new_large(pa, flags));
                return Ok(());
            }
            (*node).write(index, PageTableEntry::new_large(pa, flags));
        }
        self.mapped_frames.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    /// Map a range of physical memory, using 2 MiB pages where it can.
    ///
    /// # Why this exists
    ///
    /// The kernel has to reach *every* physical frame: the frame allocator
    /// hands out physical addresses, `kernel/syscall`'s `zero_physical`
    /// writes through a physical address directly, and the LAPIC and the
    /// framebuffer are named by physical address. Mapping all of that with
    /// 4 KiB pages would need one page table per 4 KiB of RAM — 131 072
    /// tables for a 512 MiB machine, at 8 KiB each, which is a gigabyte of
    /// page tables for half a gigabyte of memory.
    ///
    /// A 2 MiB page collapses 512 of those entries into one, so a 512 MiB
    /// identity map costs 256 page-directory entries and no leaf tables at
    /// all.
    ///
    /// # How the range is split
    ///
    /// The range is divided into three parts:
    ///
    /// * a **head** of fewer than 2 MiB, mapped 4 KiB at a time;
    /// * a **body** of whole 2 MiB chunks, mapped as large pages;
    /// * a **tail** of fewer than 2 MiB, mapped 4 KiB at a time.
    ///
    /// The head and tail exist because a large page must be 2 MiB-aligned on
    /// *both* sides: the hardware ignores the low 21 bits of the physical
    /// address in the entry, so an unaligned physical address would map the
    /// wrong frame.
    ///
    /// # Errors
    ///
    /// [`MapError::MisalignedVirtual`] if `va` is not page-aligned, and
    /// [`MapError::MisalignedPhysical`] if `pa` is not page-aligned. The
    /// range itself need not be a multiple of 2 MiB.
    pub fn map_range_large(
        &self,
        va: u64,
        pa: u64,
        len: u64,
        flags: PageFlags,
    ) -> Result<(), MapError> {
        const LARGE: u64 = 2 * 1024 * 1024;
        if !va.is_multiple_of(PAGE_SIZE) {
            return Err(MapError::MisalignedVirtual);
        }
        if !pa.is_multiple_of(PAGE_SIZE) {
            return Err(MapError::MisalignedPhysical);
        }
        if len == 0 {
            return Ok(());
        }
        let end_va = va.checked_add(len).ok_or(MapError::AddressOverflow)?;
        // The physical end is checked too, so that an overflowing range is
        // rejected before any entry is written.
        pa.checked_add(len).ok_or(MapError::AddressOverflow)?;

        // The head: up to the next 2 MiB boundary.
        let head_va_end = align_up_to(va, LARGE).min(end_va);
        let mut cursor_va = va;
        let mut cursor_pa = pa;
        while cursor_va < head_va_end {
            self.map_page(cursor_va, cursor_pa, flags)?;
            cursor_va += PAGE_SIZE;
            cursor_pa += PAGE_SIZE;
        }

        // The body: whole 2 MiB chunks, mapped as large pages.
        while cursor_va + LARGE <= end_va && cursor_pa.is_multiple_of(LARGE) {
            self.map_large_page(cursor_va, cursor_pa, flags)?;
            cursor_va += LARGE;
            cursor_pa += LARGE;
        }

        // The tail: whatever is left, mapped 4 KiB at a time.
        while cursor_va < end_va {
            self.map_page(cursor_va, cursor_pa, flags)?;
            cursor_va += PAGE_SIZE;
            cursor_pa += PAGE_SIZE;
        }
        flush();
        Ok(())
    }

    /// Walk to the page *directory* that would hold `va`'s 2 MiB leaf entry.
    fn find_directory_locked(&self, va: u64) -> Option<*mut Node> {
        let mut node = &self.root as *const Node as *mut Node;
        // SAFETY: every `children` dereference follows a pointer created under
        // the lock by `ensure_path_locked`, and the lock is held by all callers.
        unsafe {
            // The PML4 and the PDPT, then the page directory.
            for level in [
                PageLevel::PageMapLevel4,
                PageLevel::PageTableDirectoryPointer,
            ] {
                let index = level.index(va);
                if (*node).read(index).is_leaf() {
                    return None;
                }
                node = (*node).children[index].as_deref_mut()? as *mut Node;
            }
            Some(node)
        }
    }

    /// Map one 4 KiB page of `pa` at `va`.
    ///
    /// The single-page form is the primitive; [`Self::map_range`] loops it.
    /// This separation exists so the hot path has no per-page error plumbing.
    pub fn map_page(&self, va: u64, pa: u64, flags: PageFlags) -> Result<(), MapError> {
        if !va.is_multiple_of(PAGE_SIZE) {
            return Err(MapError::MisalignedVirtual);
        }
        if !pa.is_multiple_of(PAGE_SIZE) {
            return Err(MapError::MisalignedPhysical);
        }
        self.ensure_path(va);
        let _guard = self.lock.lock();
        let node = self.find_leaf_locked(va).ok_or(MapError::NotMapped)?;
        let index = PageLevel::PageTable.index(va);
        // SAFETY: the lock is held across this read-modify-write, and the
        // pointer came from a live tree node.
        unsafe {
            let existing = (*node).read(index);
            if existing.is_present() {
                if existing.physical() != pa {
                    return Err(MapError::AlreadyMapped);
                }
                // Re-mapping the same frame is how a caller updates flags, so
                // it is not an error.
                (*node).write(index, PageTableEntry(pa | encode_flags(flags)));
                return Ok(());
            }
            (*node).write(index, PageTableEntry(pa | encode_flags(flags)));
        }
        self.mapped_frames.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    /// Map `len` bytes of `pa` at `va`.
    ///
    /// Takes ownership of every frame in the range; [`Self::unmap_range`] gives
    /// them all back.
    pub fn map_range(
        &self,
        va: u64,
        pa: u64,
        len: usize,
        flags: PageFlags,
    ) -> Result<(), MapError> {
        if len == 0 || !len.is_multiple_of(PAGE_SIZE as usize) {
            return Err(MapError::MisalignedVirtual);
        }
        let end_va = va
            .checked_add(len as u64)
            .ok_or(MapError::AddressOverflow)?;
        pa.checked_add(len as u64)
            .ok_or(MapError::AddressOverflow)?;

        let pages = len as u64 / PAGE_SIZE;
        for i in 0..pages {
            // The overflow check above already proved `end_va` did not wrap,
            // so each cursor step stays inside the range.
            debug_assert!(va + i * PAGE_SIZE < end_va);
            self.map_page(va + i * PAGE_SIZE, pa + i * PAGE_SIZE, flags)?;
        }
        // One full flush beats a per-page `invlpg` for ranges this large, and
        // is required regardless: the entry may have been reused from a page
        // table that was already resident in the TLB.
        flush();
        Ok(())
    }

    /// Remove the mapping at `va`, returning the physical frame it held.
    pub fn unmap_page(&self, va: u64) -> Result<u64, MapError> {
        if !va.is_multiple_of(PAGE_SIZE) {
            return Err(MapError::MisalignedVirtual);
        }
        let _guard = self.lock.lock();
        let node = self.find_leaf_locked(va).ok_or(MapError::NotMapped)?;
        let index = PageLevel::PageTable.index(va);
        // SAFETY: as in `map_page`.
        unsafe {
            let existing = (*node).read(index);
            if !existing.is_present() {
                return Err(MapError::NotMapped);
            }
            (*node).write(index, PageTableEntry::empty());
            let pa = existing.physical();
            // `invlpg` rather than a full flush: unmapping one page is the
            // common case and must not cost a whole-TLB walk.
            invlpg(va);
            self.mapped_frames.fetch_sub(1, Ordering::Relaxed);
            Ok(pa)
        }
    }

    /// Remove every mapping in `[va, va + len)`, returning the frames in order.
    pub fn unmap_range(&self, va: u64, len: usize) -> Result<Vec<u64>, MapError> {
        if len == 0 || !len.is_multiple_of(PAGE_SIZE as usize) {
            return Err(MapError::MisalignedVirtual);
        }
        let mut freed = Vec::with_capacity(len / PAGE_SIZE as usize);
        for i in 0..(len as u64 / PAGE_SIZE) {
            freed.push(self.unmap_page(va + i * PAGE_SIZE)?);
        }
        Ok(freed)
    }

    /// Change the flags of an existing mapping without moving its frame.
    pub fn set_flags(&self, va: u64, flags: PageFlags) -> Result<(), MapError> {
        if !va.is_multiple_of(PAGE_SIZE) {
            return Err(MapError::MisalignedVirtual);
        }
        let _guard = self.lock.lock();
        let node = self.find_leaf_locked(va).ok_or(MapError::NotMapped)?;
        let index = PageLevel::PageTable.index(va);
        // SAFETY: as in `map_page`.
        unsafe {
            let existing = (*node).read(index);
            if !existing.is_present() {
                return Err(MapError::NotMapped);
            }
            (*node).write(
                index,
                PageTableEntry(existing.physical() | encode_flags(flags)),
            );
        }
        invlpg(va);
        Ok(())
    }

    /// Walk to the leaf table that would hold `va`'s entry.
    fn find_leaf_locked(&self, va: u64) -> Option<*mut Node> {
        let mut node = &self.root as *const Node as *mut Node;
        // SAFETY: every `children` dereference follows a pointer created under
        // the lock by `ensure_path_locked`, and the lock is held by all callers.
        unsafe {
            // The root is the PML4, so the walk visits three levels and lands
            // on the page table — the node whose entries are the leaves.
            // See [`INTERMEDIATE_LEVELS`].
            for level in INTERMEDIATE_LEVELS {
                let index = level.index(va);
                if (*node).read(index).is_leaf() {
                    // A leaf where a table pointer is required means this path
                    // was never built, or a large page covers it.
                    return None;
                }
                node = (*node).children[index].as_deref_mut()? as *mut Node;
            }
            Some(node)
        }
    }

    /// Translate `va` to a physical address.
    pub fn translate(&self, va: u64) -> Option<u64> {
        let _guard = self.lock.lock();
        let mut node = &self.root as *const Node as *mut Node;
        // SAFETY: as in `find_leaf_locked`.
        unsafe {
            // The root is the PML4, so this walk visits the PML4, the PDPT and
            // the page directory, and then reads the leaf from the page table.
            // See [`INTERMEDIATE_LEVELS`].
            for level in INTERMEDIATE_LEVELS {
                let entry = (*node).read(level.index(va));
                if !entry.is_present() {
                    return None;
                }
                if entry.is_large() {
                    // Large page: the entry's base is aligned to the span this
                    // level covers, so the low bits of `va` are the offset.
                    //
                    // The span of the level is `bytes_covered(level)`, and the
                    // level's own enum value is exactly that level number, so
                    // the two agree. (The page directory is level 1 and covers
                    // 2 MiB; the PDPT is level 2 and covers 1 GiB.)
                    let span = bytes_covered(level as u8);
                    return Some(entry.physical() + (va & (span - 1)));
                }
                node = (*node).children[level.index(va)].as_deref_mut()? as *mut Node;
            }
            let entry = (*node).read(PageLevel::PageTable.index(va));
            if !entry.is_present() {
                return None;
            }
            Some(entry.physical() + (va & (PAGE_SIZE - 1)))
        }
    }

    /// True when `va` falls in the kernel's higher half.
    pub fn is_kernel_address(&self, va: u64) -> bool {
        crate::paging::is_kernel_canonical(va)
    }

    /// Return this space's PML4 heap address, for `phys_of` translation.
    pub fn pml4_ptr(&self) -> *const u8 {
        self.root.table.0.as_ptr() as *const u8
    }

    /// Compute the physical PML4 base under an explicit translation.
    pub fn pml4_phys<P: PhysOfVirt>(&self, phys: &P) -> u64 {
        phys.phys_of(self.pml4_ptr())
    }

    /// The physical address of this space's PML4, using the translation the
    /// space was built with.
    ///
    /// This is the value that goes into `CR3`. It is the *same* translation
    /// the space uses for the tables below the PML4, so a space built with a
    /// real translation and a space built with the identity cannot disagree
    /// about where their own tables live.
    pub fn pml4_pa(&self) -> u64 {
        (self.phys_of)(self.pml4_ptr())
    }

    /// Set the translation this space uses for its own tables.
    ///
    /// Must be called before the space is activated, and before any mapping
    /// is made through it: a table created under one translation and a PML4
    /// read under another would describe two different trees.
    pub fn set_phys_of(&mut self, phys_of: fn(*const u8) -> u64) {
        self.phys_of = phys_of;
    }

    /// A short human-readable dump of the kernel half, for the boot banner.
    pub fn summary(&self) -> Vec<String> {
        let kernel = self.translate(KERNEL_VIRTUAL_BASE);
        let mut out = Vec::new();
        out.push(format!(
            "kernel {KERNEL_VIRTUAL_BASE:#018x} -> {}",
            kernel
                .map(|p| format!("{p:#014x}"))
                .unwrap_or_else(|| "unmapped".into())
        ));
        out.push(format!("leaf mappings: {}", self.mapped_frames()));
        out
    }

    /// Drop every table in this space.
    ///
    /// Only legal before the space is activated, because it frees the memory
    /// holding the tables CR3 points at.
    pub fn teardown(&mut self) {
        self.root = Node::new();
        self.mapped_frames.store(0, Ordering::Relaxed);
    }
}

/// Load `space` into CR3 on this CPU.
///
/// A no-op off bare metal, for the same reason as [`crate::paging::flush`].
///
/// # Safety
///
/// `space` must stay alive and unmodified for as long as this CPU runs in it,
/// and the caller must not be concurrently switching address spaces on the
/// same CPU.
pub unsafe fn activate(space: &AddressSpace) {
    if !crate::paging::is_bare_metal() {
        return;
    }
    // SAFETY: the caller guarantees the tables behind `cr3` stay valid.
    unsafe { regs::write_cr3(space.cr3()) };
    // A serialising fence stops an earlier store to the tables from being
    // reordered past the CR3 write, which would let a stale walk succeed.
    regs::mfence();
}

#[cfg(test)]
extern crate std;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paging::{Split, LEVELS};
    use std::string::String;

    /// A frame source that hands out consecutive fake frames and records every
    /// free, so accounting can be asserted exactly.
    struct FakeFrames {
        next: u64,
        budget: u64,
        freed: Vec<u64>,
    }

    impl FakeFrames {
        fn new(budget: u64) -> Self {
            Self {
                next: 0x40_0000,
                budget,
                freed: Vec::new(),
            }
        }
    }

    impl FrameSource for FakeFrames {
        fn alloc_frame(&mut self) -> Result<u64, MapError> {
            if self.budget == 0 {
                return Err(MapError::OutOfMemory);
            }
            self.budget -= 1;
            let pa = self.next;
            self.next += PAGE_SIZE;
            Ok(pa)
        }

        unsafe fn free_frame(&mut self, pa: u64) {
            self.freed.push(pa);
        }
    }

    fn space() -> AddressSpace {
        AddressSpace::new(AddressSpaceId(0x1000))
    }

    #[test]
    fn a_new_space_only_maps_the_kernel_path() {
        let s = space();
        // Tables exist along the kernel path, but no leaf data mappings do.
        assert_eq!(s.mapped_frames(), 0);
        assert!(s.is_kernel_address(KERNEL_VIRTUAL_BASE));
        assert!(!s.is_mapped(KERNEL_VIRTUAL_BASE));
    }

    #[test]
    fn kernel_half_is_reachable_and_low_half_is_not() {
        let s = space();
        // The kernel path was built, so a userspace address sharing the PML4
        // slot is still absent.
        assert!(s.translate(0x4000).is_none());
    }

    #[test]
    fn map_page_rejects_misalignment() {
        let s = space();
        assert_eq!(
            s.map_page(0x1001, 0x2000, PageFlags::kernel_rw()),
            Err(MapError::MisalignedVirtual)
        );
        assert_eq!(
            s.map_page(0x1000, 0x2001, PageFlags::kernel_rw()),
            Err(MapError::MisalignedPhysical)
        );
    }

    #[test]
    fn a_mapped_page_translates_including_its_offset() {
        let s = space();
        s.map_page(0x40_0000, 0x80_0000, PageFlags::kernel_rw())
            .unwrap();
        assert_eq!(s.translate(0x40_0000), Some(0x80_0000));
        assert_eq!(s.translate(0x40_0001), Some(0x80_0001));
        assert_eq!(s.translate(0x40_0FFF), Some(0x80_0FFF));
        assert_eq!(s.mapped_frames(), 1);
    }

    #[test]
    fn remapping_the_same_frame_updates_flags_instead_of_failing() {
        let s = space();
        s.map_page(0x40_0000, 0x80_0000, PageFlags::kernel_rw())
            .unwrap();
        // This is how a caller tightens permissions, so it must not be
        // `AlreadyMapped`.
        assert_eq!(
            s.map_page(0x40_0000, 0x80_0000, PageFlags::kernel_ro()),
            Ok(())
        );
        assert_eq!(s.translate(0x40_0000), Some(0x80_0000));
    }

    #[test]
    fn mapping_a_different_frame_over_the_same_va_is_rejected() {
        let s = space();
        s.map_page(0x40_0000, 0x80_0000, PageFlags::kernel_rw())
            .unwrap();
        assert_eq!(
            s.map_page(0x40_0000, 0x81_0000, PageFlags::kernel_rw()),
            Err(MapError::AlreadyMapped)
        );
        // The original mapping must survive the rejected attempt.
        assert_eq!(s.translate(0x40_0000), Some(0x80_0000));
    }

    #[test]
    fn unmap_returns_the_frame_and_decrements_the_count() {
        let s = space();
        s.map_page(0x40_0000, 0x80_0000, PageFlags::kernel_rw())
            .unwrap();
        assert_eq!(s.unmap_page(0x40_0000), Ok(0x80_0000));
        assert_eq!(s.mapped_frames(), 0);
        assert!(s.translate(0x40_0000).is_none());
        assert_eq!(s.unmap_page(0x40_0000), Err(MapError::NotMapped));
    }

    #[test]
    fn a_range_spanning_several_tables_translates_completely() {
        let s = space();
        let va = 0x40_0000;
        let pa = 0x100_0000;
        // 5 MiB crosses three 2 MiB boundaries, so several leaf tables are
        // needed and an off-by-one in the index math would show up here.
        let len = 5 * 1024 * 1024;
        s.map_range(va, pa, len, PageFlags::kernel_rw()).unwrap();
        let pages = len as u64 / PAGE_SIZE;
        for i in 0..pages {
            assert_eq!(
                s.translate(va + i * PAGE_SIZE),
                Some(pa + i * PAGE_SIZE),
                "page {i}"
            );
        }
        assert_eq!(s.mapped_frames() as u64, pages);
    }

    #[test]
    fn unmapping_a_range_returns_every_frame_in_order() {
        let s = space();
        s.map_range(0x40_0000, 0x100_0000, 64 * 1024, PageFlags::kernel_rw())
            .unwrap();
        let freed = s.unmap_range(0x40_0000, 64 * 1024).unwrap();
        assert_eq!(freed.len(), 16);
        for (i, pa) in freed.iter().enumerate() {
            assert_eq!(*pa, 0x100_0000 + i as u64 * PAGE_SIZE);
        }
        assert_eq!(s.mapped_frames(), 0);
    }

    #[test]
    fn range_geometry_is_validated() {
        let s = space();
        assert_eq!(
            s.map_range(0x40_0000, 0x100_0000, 0, PageFlags::kernel_rw()),
            Err(MapError::MisalignedVirtual)
        );
        assert_eq!(
            s.map_range(0x40_0000, 0x100_0000, 4097, PageFlags::kernel_rw()),
            Err(MapError::MisalignedVirtual)
        );
        assert_eq!(
            s.unmap_range(0x40_0000, 0),
            Err(MapError::MisalignedVirtual)
        );
    }

    #[test]
    fn address_overflow_is_caught_before_any_mapping() {
        let s = space();
        let huge = u64::MAX - 0xFFF;
        assert_eq!(
            s.map_range(huge, 0x1000, 4096, PageFlags::kernel_rw()),
            Err(MapError::AddressOverflow)
        );
        assert_eq!(s.mapped_frames(), 0, "a failed range must map nothing");
    }

    #[test]
    fn set_flags_keeps_the_frame() {
        let s = space();
        s.map_page(0x40_0000, 0x80_0000, PageFlags::kernel_rw())
            .unwrap();
        s.set_flags(0x40_0000, PageFlags::kernel_ro()).unwrap();
        assert_eq!(s.translate(0x40_0000), Some(0x80_0000));
        assert_eq!(
            s.set_flags(0x40_1000, PageFlags::kernel_ro()),
            Err(MapError::NotMapped)
        );
    }

    #[test]
    fn wx_request_is_never_honoured() {
        let s = space();
        s.map_page(
            0x40_0000,
            0x80_0000,
            PageFlags {
                writable: true,
                executable: true,
                ..PageFlags::kernel_rw()
            },
        )
        .unwrap();
        // The entry must have NX set, so a caller's mistaken flags cannot
        // produce an executable data page.
        let _guard = s.lock.lock();
        let node = s.find_leaf_locked(0x40_0000).unwrap();
        // SAFETY: test-only read of a live tree node under the lock.
        let entry = unsafe { (*node).read(PageLevel::PageTable.index(0x40_0000)) };
        assert_ne!(entry.0 & entry_flags::NX, 0);
    }

    #[test]
    fn user_pages_do_not_leak_into_the_kernel_half() {
        let s = space();
        s.map_page(0x40_0000, 0x80_0000, PageFlags::user_rw())
            .unwrap();
        let _guard = s.lock.lock();
        let node = s.find_leaf_locked(0x40_0000).unwrap();
        // SAFETY: test-only read of a live tree node under the lock.
        let entry = unsafe { (*node).read(PageLevel::PageTable.index(0x40_0000)) };
        assert_ne!(entry.0 & entry_flags::USER, 0);
        assert_eq!(entry.0 & entry_flags::WRITABLE, entry_flags::WRITABLE);
    }

    #[test]
    fn frame_source_exhaustion_is_reported() {
        let mut frames = FakeFrames::new(0);
        assert_eq!(frames.alloc_frame(), Err(MapError::OutOfMemory));
        let mut frames = FakeFrames::new(2);
        assert!(frames.alloc_frame().is_ok());
        assert!(frames.alloc_frame().is_ok());
        assert_eq!(frames.alloc_frame(), Err(MapError::OutOfMemory));
        // SAFETY: no address space references these fake frames.
        unsafe { frames.free_frame(0x40_0000) };
        assert_eq!(frames.freed, alloc::vec![0x40_0000]);
    }

    #[test]
    fn identity_translation_is_the_identity() {
        let s = space();
        let p = s.pml4_ptr();
        assert_eq!(IdentityPhys.phys_of(p), p as u64);
        assert_eq!(s.pml4_phys(&IdentityPhys), p as u64);
    }

    #[test]
    fn summary_mentions_the_kernel_half() {
        let s = space();
        s.map_page(0x40_0000, 0x80_0000, PageFlags::kernel_rw())
            .unwrap();
        let text = s.summary().join("\n");
        assert!(text.contains("kernel"));
        assert!(text.contains("1"));
    }

    #[test]
    fn teardown_clears_the_space() {
        let mut s = space();
        s.map_page(0x40_0000, 0x80_0000, PageFlags::kernel_rw())
            .unwrap();
        assert_eq!(s.mapped_frames(), 1);
        s.teardown();
        assert_eq!(s.mapped_frames(), 0);
        assert!(s.translate(0x40_0000).is_none());
    }

    #[test]
    fn the_walk_visits_exactly_three_levels_between_the_root_and_the_leaf() {
        // The root of an address space *is* the PML4, so the walk from the
        // root to the page table visits three levels. A fourth level would
        // descend past the page table into a table the hardware never reads,
        // and every mapping would be written where the CPU does not look.
        assert_eq!(INTERMEDIATE_LEVELS.len(), 3);
        assert_eq!(INTERMEDIATE_LEVELS[0], PageLevel::PageMapLevel4);
        assert_eq!(INTERMEDIATE_LEVELS[1], PageLevel::PageTableDirectoryPointer);
        assert_eq!(INTERMEDIATE_LEVELS[2], PageLevel::PageDirectory);
        // The page table is the leaf level, and it is *not* walked: it is
        // where the walk arrives.
        assert_eq!(
            PageLevel::PageTable.parent(),
            Some(PageLevel::PageDirectory)
        );
    }

    #[test]
    fn a_leaf_entry_lands_in_the_page_table_and_not_one_level_below_it() {
        // This is the regression test for the bug that made the kernel fault
        // on its first heap allocation: the walk descended one level too far,
        // so the leaf was written into a table the CPU never consults.
        let s = space();
        s.map_page(0x40_0000, 0x80_0000, PageFlags::kernel_rw())
            .unwrap();

        // The leaf must be reachable by reading the *page table* entry.
        let _guard = s.lock.lock();
        let node = s.find_leaf_locked(0x40_0000).unwrap();
        // SAFETY: test-only read of a live tree node under the lock.
        let entry = unsafe { (*node).read(PageLevel::PageTable.index(0x40_0000)) };
        assert_eq!(entry.physical(), 0x80_0000);
        assert!(entry.is_leaf());
    }

    #[test]
    fn every_split_index_is_in_range_for_the_tables_we_walk() {
        // The tree walk indexes with 9-bit fields; a mistake there is a memory
        // safety bug, so it gets its own test across the whole low half.
        for va in (0u64..(1u64 << 30)).step_by(0x20_0000) {
            for level in LEVELS {
                assert!(level.index(va) < ENTRIES_PER_TABLE);
            }
            assert!(Split::of(va).offset < PAGE_SIZE);
        }
        let text: String = format!("{:?}", space());
        assert!(text.contains("AddressSpace"));
    }
}
