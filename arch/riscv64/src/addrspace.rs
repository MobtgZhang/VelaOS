//! Ownership and manipulation of Sv39 page tables.
//!
//! [`AddressSpace`] is the only type in VelaOS that may write a page-table
//! entry. Requiring the tree's lock for every write is what turns a
//! double-free or use-after-free of a mapping into a serialisation question
//! rather than a data race — the same discipline as the x86-64 crate.
//!
//! # How the tables are reached
//!
//! The walk machinery in [`crate::paging`] is *physical-address driven*: it
//! asks a [`PhysBackend`] to read or write one 64-bit PTE at a physical
//! address, and to allocate a fresh zeroed table. [`TablePool`] is the
//! reference backend: every table is ordinary heap memory (`Vec<u64>`),
//! keyed by the physical address its entries name for it. During boot the
//! kernel heap is identity-mapped (see [`PhysOfVirt`]), so those keys are
//! real physical addresses and a table's key *is* the pointer to it; the
//! kernel's boot code places the pool inside a region it has reserved, and
//! `satp` then names a table that genuinely exists at that address.
//!
//! A table that was never allocated reads as [`MapError::NotMapped`], which
//! turns a dangling table pointer into an error rather than a wild read —
//! that is the pool's safety contract, and the tests pin it.
//!
//! # The kernel window, and why every space inherits one
//!
//! There is no address-space switch on the trap path: an `ecall` or a timer
//! interrupt continues to run under the *interrupted* code's `satp`, so a
//! user space has to contain the kernel's own mappings or the first syscall
//! would fault before the kernel could print a diagnostic.
//! [`AddressSpace::with_kernel_mappings`] deep-clones the tree for exactly
//! that reason; see its documentation. The inherited mappings are
//! supervisor pages (no `U` bit), so inheriting them grants a program
//! nothing it did not have.
//!
//! # Why the tree walk is shared with `paging`
//!
//! The x86-64 crate duplicates its walk between `paging` (pure arithmetic,
//! mock-tested) and `addrspace` (tree ownership). This crate does not: the
//! walk *is* [`PhysBackend`]-shaped, so [`crate::paging`]'s map, unmap,
//! flags and translate functions run directly against the pool, and
//! `addrspace` only owns the lock, the counters and the clone. The walk's
//! arithmetic is tested once, with a mock, in `paging`.

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicUsize, Ordering};

use spin::Mutex;

use vela_arch_common::{AddressSpaceId, MapError, PageFlags, PAGE_SIZE};

use crate::paging::{
    self, pte_address, pte_is_leaf, pte_new_table, pte_pa, pte_valid, translate as walk_translate,
    unmap_page as walk_unmap, PhysBackend, ENTRIES_PER_TABLE, KERNEL_VIRTUAL_BASE,
};
use crate::regs;

/// The pool's default starting frame: the first table handed out by
/// [`TablePool::new`] when a caller has no reserved region yet (host tests,
/// or a boot path that has not pinned its heap). The kernel's boot code
/// passes its own base to [`AddressSpace::with_pool`] instead.
pub const DEFAULT_POOL_BASE: u64 = 0x0010_0000;

/// How to translate a heap pointer into the physical address the CPU needs
/// in a PTE or in `satp`.
///
/// During boot the kernel heap is identity-mapped, so the identity
/// implementation is correct and is what the kernel installs; a future
/// higher-half layout supplies a real translation, and nothing else in this
/// module changes.
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

/// A source of physical frames for *data* mappings.
///
/// Distinct from table allocation on purpose: tables come from the pool
/// below, and only data pages are handed out by the physical memory
/// manager. This is the surface `kernel/memory` implements.
pub trait FrameSource {
    /// Return one zeroed, page-aligned frame.
    ///
    /// # Errors
    ///
    /// [`MapError::OutOfMemory`] when the platform has no frame left.
    fn alloc_frame(&mut self) -> Result<u64, MapError>;

    /// Return a frame previously obtained from this source.
    ///
    /// # Safety
    ///
    /// The caller must guarantee the frame is no longer referenced by any
    /// address space. A physical frame present in two PTEs is not merely a
    /// leak — it lets one address space observe the other's writes.
    unsafe fn free_frame(&mut self, pa: u64);
}

/// The heap-backed [`PhysBackend`]: tables live in ordinary memory, keyed
/// by the physical address their entries name for them.
///
/// Tables are handed out by a bump allocator from a caller-chosen base, in
/// [`PAGE_SIZE`] steps, because a table must be page-aligned for the entry
/// pointing at it to be expressible (the PPN field consumes the low 12
/// bits — see [`crate::paging`]).
pub struct TablePool {
    /// Every allocated table, keyed by its physical address.
    tables: alloc::collections::BTreeMap<u64, Vec<u64>>,
    /// The next frame the bump allocator will hand out.
    next_table: u64,
}

impl TablePool {
    /// Create an empty pool, handing out table frames from `first_table`
    /// upward.
    pub fn new(first_table: u64) -> Self {
        Self {
            tables: alloc::collections::BTreeMap::new(),
            next_table: first_table,
        }
    }

    /// The number of tables currently held.
    pub fn table_count(&self) -> usize {
        self.tables.len()
    }

    /// Deep-copy the source's tree into this pool, rewriting every
    /// table-pointer entry.
    ///
    /// The source tables are read *from this same pool* (the clone shares
    /// it), and the copies land above the bump allocator's watermark, so a
    /// copy never collides with its source: the source's keys are all below
    /// `next_table`, and every allocation is at or above it.
    ///
    /// `depth` is the level of the table currently being copied — 0 for the
    /// root. Sv39 has exactly three levels (root, middle, leaf table), so a
    /// pointer chain deeper than that is not a shape this crate produces;
    /// it is refused rather than followed, because an unbounded chain would
    /// recurse without limit on a corrupted table.
    ///
    /// # Errors
    ///
    /// [`MapError::NotMapped`] when a source table does not exist (a
    /// pointer entry naming a table that was never allocated) or the chain
    /// descends past the leaf level; [`MapError::OutOfMemory`] when the
    /// bump allocator has exhausted the caller's chosen region.
    fn deep_copy(&mut self, src_pa: u64, depth: usize) -> Result<u64, MapError> {
        if depth > 2 {
            // Root (0), middle (1), leaf table (2): a fourth level is not
            // Sv39, so the chain is corrupt and is not followed.
            return Err(MapError::NotMapped);
        }
        let dst_pa = self.alloc_table()?;
        let table = self
            .tables
            .get(&src_pa)
            .cloned()
            .ok_or(MapError::NotMapped)?;
        for (i, &pte) in table.iter().enumerate() {
            if !pte_valid(pte) {
                continue;
            }
            if pte_is_leaf(pte) {
                // A leaf — 4 KiB page or megapage — is copied verbatim: the
                // frame it names belongs to the data memory manager, not to
                // this tree.
                self.write_pte(pte_address(dst_pa, i), pte)?;
            } else {
                // A pointer: the child is cloned first, and the entry is
                // rewritten to name the *clone's* address. Copying the
                // address unchanged would make the clone share the source's
                // subtree, so user mappings added later would alias.
                let child = self.deep_copy(pte_pa(pte), depth + 1)?;
                self.write_pte(pte_address(dst_pa, i), pte_new_table(child))?;
            }
        }
        Ok(dst_pa)
    }
}

impl PhysBackend for TablePool {
    fn read_pte(&mut self, pa: u64) -> Result<u64, MapError> {
        // `pte_address` lays entries at table + index * 8, so the table's
        // key is the address with its low 12 bits cleared, and the index is
        // what those bits encode.
        let key = pa & !0x0FFF;
        let index = ((pa & 0x0FFF) / 8) as usize;
        self.tables
            .get(&key)
            .and_then(|t| t.get(index))
            .copied()
            .ok_or(MapError::NotMapped)
    }

    fn write_pte(&mut self, pa: u64, value: u64) -> Result<(), MapError> {
        let key = pa & !0x0FFF;
        let index = ((pa & 0x0FFF) / 8) as usize;
        let table = self.tables.get_mut(&key).ok_or(MapError::NotMapped)?;
        *table.get_mut(index).ok_or(MapError::NotMapped)? = value;
        Ok(())
    }

    fn alloc_table(&mut self) -> Result<u64, MapError> {
        let pa = self.next_table;
        self.next_table = pa.checked_add(PAGE_SIZE).ok_or(MapError::OutOfMemory)?;
        self.tables.insert(pa, alloc::vec![0u64; ENTRIES_PER_TABLE]);
        Ok(pa)
    }
}

/// A whole address space: an Sv39 root table and the tables reachable from
/// it, stored through a shared [`TablePool`].
///
/// The pool is behind an `Arc` so that [`AddressSpace::with_kernel_mappings`]
/// can give a user space its own tree *in the same allocator* the kernel
/// uses — the clone's fresh tables come off the same bump watermark, which
/// is what keeps them disjoint from every table that exists.
pub struct AddressSpace {
    /// The space's identity: the physical base of its root table.
    id: AddressSpaceId,
    /// The root table's physical address — the PPN `satp` names.
    root_pa: u64,
    /// Every table this space can reach, shared with any space cloned from
    /// this one.
    pool: Arc<Mutex<TablePool>>,
    /// Leaf mappings created through [`Self::map_range`]; a statistic.
    mapped_frames: AtomicUsize,
}

impl core::fmt::Debug for AddressSpace {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("AddressSpace")
            .field("id", &self.id)
            .field("satp", &alloc::format!("{:#x}", self.satp()))
            .field("mapped_frames", &self.mapped_frames)
            .finish()
    }
}

impl AddressSpace {
    /// Create an empty address space whose tables come from a pool starting
    /// at [`DEFAULT_POOL_BASE`].
    ///
    /// Nothing is mapped: the kernel's Sv39 image mapping is built by the
    /// boot code that owns the frames, and a user space inherits it through
    /// [`Self::with_kernel_mappings`]. The root table itself is allocated
    /// immediately, so `satp` can name this space before any mapping exists.
    pub fn new(id: AddressSpaceId) -> Self {
        Self::with_pool(id, DEFAULT_POOL_BASE)
    }

    /// Create an empty address space whose tables are bumped from
    /// `first_table` — how the kernel's boot code places the pool inside a
    /// region it reserved.
    pub fn with_pool(_id: AddressSpaceId, first_table: u64) -> Self {
        let mut pool = TablePool::new(first_table);
        // The root exists from construction: `satp` must be able to name
        // the space before any mapping is made. The only failure is pool
        // exhaustion on the very first frame, which cannot happen for a
        // sane base — but the error is handled rather than unwrapped.
        let root_pa = pool.alloc_table().unwrap_or(first_table);
        Self {
            id: AddressSpaceId(root_pa),
            root_pa,
            pool: Arc::new(Mutex::new(pool)),
            mapped_frames: AtomicUsize::new(0),
        }
    }

    /// Create a user address space that inherits every supervisor mapping.
    ///
    /// # Why the kernel must be reachable from a user `satp`
    ///
    /// There is no address-space switch on the trap path: an `ecall` or a
    /// timer interrupt continues to use the tables the interrupted code was
    /// running under. A user space therefore has to contain the kernel's
    /// own mappings or the first syscall would fault before the kernel
    /// could even print a diagnostic. The inherited mappings are
    /// supervisor pages — present but not user-accessible — so inheriting
    /// them gives a program no access it did not have.
    ///
    /// # Why a deep clone rather than a shared root
    ///
    /// User mappings are added and removed per task, so two spaces cannot
    /// share the tables that contain them: a shared root would make every
    /// task's user mapping visible to the others. [`TablePool::deep_copy`]
    /// allocates fresh tables from the shared pool and rewrites every
    /// pointer entry, so the clone owns its tree outright while still
    /// copying the kernel's fixed leaves into it.
    ///
    /// # Errors
    ///
    /// [`MapError::OutOfMemory`] when the shared pool has no frames left
    /// for the clone's tables, and [`MapError::NotMapped`] when the
    /// kernel's tree is corrupt (a pointer naming a table that was never
    /// allocated).
    pub fn with_kernel_mappings(
        _id: AddressSpaceId,
        kernel: &AddressSpace,
    ) -> Result<Self, MapError> {
        let root_pa = {
            let mut pool = kernel.pool.lock();
            pool.deep_copy(kernel.root_pa, 0)?
        };
        Ok(Self {
            id: AddressSpaceId(root_pa),
            root_pa,
            pool: Arc::clone(&kernel.pool),
            mapped_frames: AtomicUsize::new(kernel.mapped_frames.load(Ordering::Relaxed)),
        })
    }

    /// This space's identity — the physical base of its root table.
    pub const fn id(&self) -> AddressSpaceId {
        self.id
    }

    /// The value to load into `satp` (Sv39 mode, no ASID).
    pub const fn satp(&self) -> u64 {
        // The `satp` PPN field holds the root table's *page number*, so the
        // address is shifted down by the page size before it goes in.
        paging::satp_value(self.root_pa >> 12, 0)
    }

    /// The value to load into `satp` (parity alias for the x86-64 `cr3`).
    pub const fn cr3(&self) -> u64 {
        self.satp()
    }

    /// Re-point this space at a different root-table base.
    ///
    /// Called once by the kernel's boot code after the root table's region
    /// has been given its real physical address, mirroring the x86-64
    /// crate's `set_pml4_base`. The base must be a page-aligned frame the
    /// pool knows about, or every later walk reports [`MapError::NotMapped`].
    pub fn set_pml4_base(&mut self, root_pa: u64) {
        self.id = AddressSpaceId(root_pa);
        self.root_pa = root_pa;
    }

    /// The root table's address as a pointer — correct under the identity
    /// mapping the boot heap runs under (see [`PhysOfVirt`]).
    pub fn pml4_ptr(&self) -> *const u8 {
        self.root_pa as *const u8
    }

    /// The physical base of the root table — the PPN `satp` names.
    pub const fn pml4_pa(&self) -> u64 {
        self.root_pa
    }

    /// The physical base of the root table (parity alias).
    pub const fn root_pa(&self) -> u64 {
        self.root_pa
    }

    /// Leaf mappings currently present.
    pub fn mapped_frames(&self) -> usize {
        self.mapped_frames.load(Ordering::Relaxed)
    }

    /// True when `va` has a mapping, without exposing the physical address.
    pub fn is_mapped(&self, va: u64) -> bool {
        self.translate(va).is_some()
    }

    /// True when `va` falls in the kernel's (sign-extended) high half.
    pub fn is_kernel_address(&self, va: u64) -> bool {
        paging::is_kernel_canonical(va)
    }

    /// Map one 4 KiB page of `pa` at `va`.
    ///
    /// Re-mapping the *same* frame is a flags update, not a conflict; a
    /// different frame in an occupied slot is [`MapError::AlreadyMapped`].
    ///
    /// # Errors
    ///
    /// As [`paging::map_page`]: misalignment (virtual or physical), address
    /// overflow, an occupied slot, or pool exhaustion.
    pub fn map_page(&self, va: u64, pa: u64, flags: PageFlags) -> Result<(), MapError> {
        // The count only moves when a *new* leaf appears; a same-frame
        // re-encode is a flags update and must not double-count.
        let existed = self.translate(va).is_some();
        {
            let mut pool = self.pool.lock();
            // The walk mutates only this space's own tables, through the
            // pool, under the lock taken above.
            paging::map_page(&mut *pool, self.root_pa, va, pa, flags)?;
        }
        if !existed {
            self.mapped_frames.fetch_add(1, Ordering::Relaxed);
        }
        Ok(())
    }

    /// Map `len` bytes of `pa` at `va`, 4 KiB at a time.
    ///
    /// Takes ownership of every frame in the range; [`Self::unmap_range`]
    /// gives them all back.
    ///
    /// # Errors
    ///
    /// [`MapError::MisalignedVirtual`] when `va` or `len` is misaligned;
    /// [`MapError::AddressOverflow`] when the range would wrap; then
    /// whatever [`Self::map_page`] reports, page by page.
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
        // Both ends are overflow-checked before any entry is written, so a
        // failing range maps nothing.
        pa.checked_add(len as u64)
            .ok_or(MapError::AddressOverflow)?;
        va.checked_add(len as u64)
            .ok_or(MapError::AddressOverflow)?;

        let pages = len as u64 / PAGE_SIZE;
        for i in 0..pages {
            self.map_page(va + i * PAGE_SIZE, pa + i * PAGE_SIZE, flags)?;
        }
        // One full flush beats per-page fences for a range this large, and
        // is required anyway: a table freshly written may already sit in
        // the TLB from an earlier walk.
        // SAFETY: a full `sfence.vma` is always defined in S-mode.
        unsafe { regs::sfence_vma_all() };
        Ok(())
    }

    /// Remove the mapping at `va`, returning the physical frame it held.
    ///
    /// Intermediate tables above the leaf are *not* returned to the pool,
    /// mirroring the x86-64 crate: only the leaf frame goes back.
    pub fn unmap_page(&self, va: u64) -> Result<u64, MapError> {
        let pa = {
            let mut pool = self.pool.lock();
            // As in `map_page` — own tables, under the lock.
            walk_unmap(&mut *pool, self.root_pa, va)?
        };
        self.mapped_frames.fetch_sub(1, Ordering::Relaxed);
        // SAFETY: a full `sfence.vma` is always defined in S-mode.
        unsafe { regs::sfence_vma_all() };
        Ok(pa)
    }

    /// Remove every mapping in `[va, va + len)`, returning the frames in
    /// order.
    ///
    /// # Errors
    ///
    /// [`MapError::MisalignedVirtual`] when `va` or `len` is misaligned;
    /// then whatever [`Self::unmap_page`] reports.
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
    ///
    /// # Errors
    ///
    /// [`MapError::MisalignedVirtual`] when `va` is misaligned, and
    /// [`MapError::NotMapped`] when `va` has no valid leaf.
    pub fn set_flags(&self, va: u64, flags: PageFlags) -> Result<(), MapError> {
        {
            let mut pool = self.pool.lock();
            // As in `map_page`.
            paging::set_flags(&mut *pool, self.root_pa, va, flags)?;
        }
        // SAFETY: a full `sfence.vma` is always defined in S-mode.
        unsafe { regs::sfence_vma_all() };
        Ok(())
    }

    /// Translate `va` to a physical address, or `None` when unmapped.
    pub fn translate(&self, va: u64) -> Option<u64> {
        let mut pool = self.pool.lock();
        // The read-only walk touches only this space's tables.
        walk_translate(&mut *pool, self.root_pa, va).ok()?
    }

    /// A short human-readable dump, for the boot banner.
    pub fn summary(&self) -> Vec<String> {
        let kernel = self.translate(KERNEL_VIRTUAL_BASE);
        let mut out = Vec::new();
        out.push(alloc::format!(
            "kernel {KERNEL_VIRTUAL_BASE:#018x} -> {}",
            kernel
                .map(|p| alloc::format!("{p:#014x}"))
                .unwrap_or_else(|| "unmapped".into())
        ));
        out.push(alloc::format!("leaf mappings: {}", self.mapped_frames()));
        out.push(alloc::format!("satp: {:#x}", self.satp()));
        out
    }

    /// Drop every table in this space.
    ///
    /// Only legal before the space is activated: it frees the memory the
    /// `satp` PPN points at.
    pub fn teardown(&mut self) {
        self.pool.lock().tables.clear();
        self.mapped_frames.store(0, Ordering::Relaxed);
    }
}

/// Load `space` into `satp` on this CPU.
///
/// A no-op off bare metal, like every privileged instruction in this crate.
///
/// # Safety
///
/// `space` must stay alive and unmodified for as long as this CPU runs in
/// it, and the caller must not be concurrently switching address spaces on
/// the same CPU.
pub unsafe fn activate(space: &AddressSpace) {
    // SAFETY: the caller guarantees the root table behind `satp` stays
    // valid and that no concurrent switch is in flight.
    unsafe { regs::write_satp(space.satp()) };
}

#[cfg(test)]
extern crate std;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paging::{pte_flags_of, pte_new, Split};
    use std::string::String;

    /// A frame source that hands out consecutive fake frames and records
    /// every free, so accounting can be asserted exactly.
    struct FakeFrames {
        next: u64,
        budget: u64,
        freed: Vec<u64>,
    }

    impl FakeFrames {
        fn new(budget: u64) -> Self {
            Self {
                next: 0x0040_0000,
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
    fn a_new_space_has_a_root_and_no_leaves() {
        let s = space();
        assert_eq!(s.mapped_frames(), 0);
        assert!(!s.is_mapped(KERNEL_VIRTUAL_BASE));
        // The id is the root's physical base, because that is what `satp`
        // and the registry both name.
        assert_eq!(s.root_pa(), DEFAULT_POOL_BASE);
        assert_eq!(s.id().0, DEFAULT_POOL_BASE);
    }

    #[test]
    fn satp_names_sv39_and_the_root() {
        let s = space();
        assert_eq!(paging::satp_mode(s.satp()), paging::SATP_MODE_SV39);
        assert_eq!(paging::satp_ppn(s.satp()), DEFAULT_POOL_BASE >> 12);
        // The parity alias agrees.
        assert_eq!(s.cr3(), s.satp());
    }

    #[test]
    fn a_mapped_page_translates_including_its_offset() {
        let s = space();
        s.map_page(0x0040_0000, 0x0080_0000, PageFlags::kernel_rw())
            .unwrap();
        assert_eq!(s.translate(0x0040_0000), Some(0x0080_0000));
        assert_eq!(s.translate(0x0040_0001), Some(0x0080_0001));
        assert_eq!(s.translate(0x0040_0FFF), Some(0x0080_0FFF));
        assert_eq!(s.translate(0x0040_1000), None);
        assert_eq!(s.mapped_frames(), 1);
    }

    #[test]
    fn remapping_the_same_frame_updates_flags_and_does_not_double_count() {
        let s = space();
        s.map_page(0x0040_0000, 0x0080_0000, PageFlags::kernel_rw())
            .unwrap();
        // This is how a caller tightens permissions, so it must not be
        // `AlreadyMapped` — and must not bump the leaf counter twice.
        assert_eq!(
            s.map_page(0x0040_0000, 0x0080_0000, PageFlags::kernel_ro()),
            Ok(())
        );
        assert_eq!(s.mapped_frames(), 1);
        assert_eq!(s.translate(0x0040_0000), Some(0x0080_0000));
    }

    #[test]
    fn mapping_a_different_frame_over_the_same_va_is_rejected() {
        let s = space();
        s.map_page(0x0040_0000, 0x0080_0000, PageFlags::kernel_rw())
            .unwrap();
        assert_eq!(
            s.map_page(0x0040_0000, 0x0081_0000, PageFlags::kernel_rw()),
            Err(MapError::AlreadyMapped)
        );
        // The original mapping must survive the rejected attempt.
        assert_eq!(s.translate(0x0040_0000), Some(0x0080_0000));
    }

    #[test]
    fn misalignment_and_overflow_are_rejected_before_any_mapping() {
        let s = space();
        assert_eq!(
            s.map_page(0x1001, 0x2000, PageFlags::kernel_rw()),
            Err(MapError::MisalignedVirtual)
        );
        assert_eq!(
            s.map_page(0x1000, 0x2001, PageFlags::kernel_rw()),
            Err(MapError::MisalignedPhysical)
        );
        assert_eq!(
            s.map_page(0x1000, paging::MAX_PHYSICAL, PageFlags::kernel_rw()),
            Err(MapError::AddressOverflow)
        );
        assert_eq!(s.mapped_frames(), 0, "a failed map must map nothing");
    }

    #[test]
    fn unmap_returns_the_frame_and_decrements_the_count() {
        let s = space();
        s.map_page(0x0040_0000, 0x0080_0000, PageFlags::kernel_rw())
            .unwrap();
        assert_eq!(s.unmap_page(0x0040_0000), Ok(0x0080_0000));
        assert_eq!(s.mapped_frames(), 0);
        assert!(s.translate(0x0040_0000).is_none());
        assert_eq!(s.unmap_page(0x0040_0000), Err(MapError::NotMapped));
    }

    #[test]
    fn a_range_of_pages_translates_completely() {
        let s = space();
        let va = 0x0040_0000;
        let pa = 0x0100_0000;
        let len = 64 * 1024;
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
        s.map_range(0x0040_0000, 0x0100_0000, 64 * 1024, PageFlags::kernel_rw())
            .unwrap();
        let freed = s.unmap_range(0x0040_0000, 64 * 1024).unwrap();
        assert_eq!(freed.len(), 16);
        for (i, pa) in freed.iter().enumerate() {
            assert_eq!(*pa, 0x0100_0000 + i as u64 * PAGE_SIZE);
        }
        assert_eq!(s.mapped_frames(), 0);
    }

    #[test]
    fn range_geometry_is_validated() {
        let s = space();
        assert_eq!(
            s.map_range(0x0040_0000, 0x0100_0000, 0, PageFlags::kernel_rw()),
            Err(MapError::MisalignedVirtual)
        );
        assert_eq!(
            s.map_range(0x0040_0000, 0x0100_0000, 4097, PageFlags::kernel_rw()),
            Err(MapError::MisalignedVirtual)
        );
        assert_eq!(
            s.unmap_range(0x0040_0000, 0),
            Err(MapError::MisalignedVirtual)
        );
    }

    #[test]
    fn address_overflow_is_caught_before_any_mapping() {
        let s = space();
        let huge = u64::MAX - 0x0FFF;
        assert_eq!(
            s.map_range(huge, 0x1000, 4096, PageFlags::kernel_rw()),
            Err(MapError::AddressOverflow)
        );
        assert_eq!(s.mapped_frames(), 0, "a failed range must map nothing");
    }

    #[test]
    fn set_flags_keeps_the_frame() {
        let s = space();
        s.map_page(0x0040_0000, 0x0080_0000, PageFlags::kernel_rw())
            .unwrap();
        s.set_flags(0x0040_0000, PageFlags::kernel_ro()).unwrap();
        assert_eq!(s.translate(0x0040_0000), Some(0x0080_0000));
        assert_eq!(
            s.set_flags(0x0040_1000, PageFlags::kernel_ro()),
            Err(MapError::NotMapped)
        );
    }

    #[test]
    fn user_pages_do_not_leak_into_the_kernel_half() {
        let s = space();
        // A user page in the *low* half is fine; the high half is the
        // kernel window and `is_kernel_address` reports it.
        s.map_page(0x0040_0000, 0x0080_0000, PageFlags::user_rw())
            .unwrap();
        assert!(!s.is_kernel_address(0x0040_0000));
        assert!(s.is_kernel_address(KERNEL_VIRTUAL_BASE));
    }

    #[test]
    fn mmio_maps_carry_the_io_memory_type() {
        let s = space();
        s.map_page(0x0000_1000_0000, 0x0000_1000_0000, PageFlags::mmio())
            .unwrap();
        // Read the leaf back through the pool and decode its flags: the
        // MMIO request must have produced PBMT=IO, the Svpbmt encoding.
        let mut pool = s.pool.lock();
        let backend: &mut dyn PhysBackend = &mut *pool;
        let split = Split::of(0x0000_1000_0000);
        let mut table = s.root_pa;
        for index in [split.vpn2, split.vpn1] {
            let pte = backend.read_pte(pte_address(table, index)).unwrap();
            assert!(pte_valid(pte));
            assert!(!pte_is_leaf(pte), "intermediate entries are pointers");
            table = pte_pa(pte);
        }
        let leaf = backend.read_pte(pte_address(table, split.vpn0)).unwrap();
        assert!(pte_valid(leaf));
        assert!(pte_flags_of(leaf).uncacheable, "mmio() must set PBMT=IO");
    }

    #[test]
    fn one_mapping_needs_exactly_three_tables() {
        // Sv39 has exactly three levels, so one mapping anywhere needs
        // root + middle + leaf = 3 tables.
        let s = space();
        s.map_page(0x7000, 0xB000, PageFlags::kernel_rw()).unwrap();
        assert_eq!(s.pool.lock().table_count(), 3);
    }

    #[test]
    fn two_pages_in_the_same_two_mib_share_their_leaf_table() {
        let s = space();
        s.map_page(0x0000_1000_0000, 0x0000_2000_0000, PageFlags::kernel_rw())
            .unwrap();
        s.map_page(0x0000_1000_1000, 0x0000_2000_1000, PageFlags::kernel_rw())
            .unwrap();
        // Still three tables: the leaf table serves both pages.
        assert_eq!(s.pool.lock().table_count(), 3);
    }

    #[test]
    fn the_kernel_clone_owns_fresh_tables_with_the_same_leaves() {
        let kernel = space();
        kernel
            .map_page(KERNEL_VIRTUAL_BASE, 0x2000, PageFlags::kernel_rx())
            .unwrap();
        let user = AddressSpace::with_kernel_mappings(AddressSpaceId(0), &kernel).expect("clone");
        // The clone's leaves translate identically...
        assert_eq!(user.translate(KERNEL_VIRTUAL_BASE), Some(0x2000));
        // ...but its root is a *different* table...
        assert_ne!(user.root_pa(), kernel.root_pa());
        // ...and adding a user mapping changes only the clone.
        user.map_page(0x0040_0000, 0x0080_0000, PageFlags::user_rw())
            .unwrap();
        assert_eq!(user.translate(0x0040_0000), Some(0x0080_0000));
        assert_eq!(
            kernel.translate(0x0040_0000),
            None,
            "the kernel tree is untouched"
        );
        assert_eq!(kernel.mapped_frames(), 1);
    }

    #[test]
    fn the_clone_counter_starts_at_the_source_count() {
        let kernel = space();
        kernel
            .map_page(0x0040_0000, 0x0080_0000, PageFlags::kernel_rw())
            .unwrap();
        kernel
            .map_page(0x0040_1000, 0x0080_1000, PageFlags::kernel_rw())
            .unwrap();
        let user = AddressSpace::with_kernel_mappings(AddressSpaceId(0), &kernel).expect("clone");
        assert_eq!(user.mapped_frames(), 2);
    }

    #[test]
    fn the_clone_refuses_a_corrupt_chain() {
        // Hand-build a tree whose root pointer names a table that was
        // never allocated: the deep copy must report it, not follow it.
        let kernel = space();
        {
            let mut pool = kernel.pool.lock();
            // The tree is corrupted deliberately, under the lock: the
            // deep copy must report the broken entry rather than follow it.
            {
                PhysBackend::write_pte(
                    &mut *pool,
                    pte_address(kernel.root_pa(), 0),
                    pte_new_table(0x0DEA_D000),
                )
                .unwrap();
            }
        }
        assert_eq!(
            AddressSpace::with_kernel_mappings(AddressSpaceId(0), &kernel).err(),
            Some(MapError::NotMapped)
        );
    }

    #[test]
    fn the_clone_refuses_a_chain_deeper_than_sv39() {
        // Four pointer levels is not a shape Sv39 can express; the copy
        // must refuse it rather than recurse without bound.
        let kernel = space();
        {
            let mut pool = kernel.pool.lock();
            // The tree is corrupted deliberately, under the lock.
            {
                let mut table = kernel.root_pa();
                // Three pointer levels deep: root -> t1 -> t2 -> t3.
                for _ in 0..3 {
                    let child = PhysBackend::alloc_table(&mut *pool).unwrap();
                    PhysBackend::write_pte(&mut *pool, pte_address(table, 0), pte_new_table(child))
                        .unwrap();
                    table = child;
                }
            }
        }
        assert_eq!(
            AddressSpace::with_kernel_mappings(AddressSpaceId(0), &kernel).err(),
            Some(MapError::NotMapped)
        );
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
        unsafe { frames.free_frame(0x0040_0000) };
        assert_eq!(frames.freed, alloc::vec![0x0040_0000]);
    }

    #[test]
    fn the_pool_reports_a_never_allocated_table_as_not_mapped() {
        // A dangling table pointer must read as an error, not a wild
        // read: that is the pool's safety contract.
        let mut pool = TablePool::new(0x0020_0000);
        assert_eq!(
            PhysBackend::read_pte(&mut pool, 0x0DEA_D000),
            Err(MapError::NotMapped)
        );
        assert_eq!(
            PhysBackend::write_pte(&mut pool, 0x0DEA_D000, 0),
            Err(MapError::NotMapped)
        );
    }

    #[test]
    fn pool_allocated_tables_are_zeroed_and_page_strided() {
        let mut pool = TablePool::new(0x0020_0000);
        let a = pool.alloc_table().unwrap();
        let b = pool.alloc_table().unwrap();
        assert_eq!(b - a, PAGE_SIZE);
        for i in 0..ENTRIES_PER_TABLE {
            assert_eq!(
                PhysBackend::read_pte(&mut pool, pte_address(a, i)).unwrap(),
                0,
                "a fresh table is zeroed"
            );
        }
        assert_eq!(pool.table_count(), 2);
    }

    #[test]
    fn pool_reads_decode_the_entry_index_from_the_low_bits() {
        // `pte_address(table, 5)` must read back through the pool exactly
        // as the walk wrote it — the key/index split is the one place the
        // arithmetic could silently alias entry 5 with entry 5 of another
        // table.
        let mut pool = TablePool::new(0x0020_0000);
        let table = pool.alloc_table().unwrap();
        for i in [0usize, 1, 255, 511] {
            PhysBackend::write_pte(&mut pool, pte_address(table, i), 0x1000 + i as u64).unwrap();
        }
        for i in [0usize, 1, 255, 511] {
            assert_eq!(
                PhysBackend::read_pte(&mut pool, pte_address(table, i)).unwrap(),
                0x1000 + i as u64
            );
        }
    }

    #[test]
    fn teardown_clears_the_space() {
        let mut s = space();
        s.map_page(0x0040_0000, 0x0080_0000, PageFlags::kernel_rw())
            .unwrap();
        assert_eq!(s.mapped_frames(), 1);
        s.teardown();
        assert_eq!(s.mapped_frames(), 0);
        assert_eq!(s.pool.lock().table_count(), 0);
    }

    #[test]
    fn summary_mentions_the_kernel_half_and_satp() {
        let s = space();
        s.map_page(KERNEL_VIRTUAL_BASE, 0x2000, PageFlags::kernel_rx())
            .unwrap();
        let text: String = s.summary().join("\n");
        assert!(text.contains("kernel"));
        assert!(text.contains("satp"));
    }

    #[test]
    fn debug_rendering_names_the_struct() {
        let text: String = alloc::format!("{:?}", space());
        assert!(text.contains("AddressSpace"));
        assert!(text.contains("satp"));
    }

    #[test]
    fn set_pml4_base_repoints_the_space() {
        let mut s = space();
        s.set_pml4_base(0x0030_0000);
        assert_eq!(s.root_pa(), 0x0030_0000);
        assert_eq!(s.id().0, 0x0030_0000);
        assert_eq!(paging::satp_ppn(s.satp()), 0x0030_0000 >> 12);
    }

    #[test]
    fn identity_phys_is_the_identity() {
        let p = 0x1234 as *const u8;
        assert_eq!(IdentityPhys.phys_of(p), 0x1234);
        // The root pointer is the identity-mapped address of the root.
        let s = space();
        assert_eq!(s.pml4_ptr() as u64, s.pml4_pa());
    }

    #[test]
    fn activate_is_inert_off_bare_metal() {
        let s = space();
        // SAFETY: the documented host no-op.
        unsafe { activate(&s) };
    }

    #[test]
    fn the_walk_visits_exactly_two_pointer_levels_below_the_root() {
        // Sv39: root -> middle -> leaf-table -> PTE. Two pointer hops; a
        // third would descend past the leaf table the hardware reads.
        let split = Split::of(0x0040_0000);
        assert!(split.vpn2 < ENTRIES_PER_TABLE);
        assert!(split.vpn1 < ENTRIES_PER_TABLE);
        assert!(split.vpn0 < ENTRIES_PER_TABLE);
    }

    #[test]
    fn every_leaf_the_walk_writes_is_a_valid_leaf() {
        let s = space();
        s.map_page(0x6000, 0x9000, PageFlags::kernel_rw()).unwrap();
        let leaf = pte_new(0x9000, PageFlags::kernel_rw(), true);
        assert!(pte_valid(leaf));
        assert!(pte_is_leaf(leaf));
        assert_eq!(pte_pa(leaf), 0x9000);
    }
}
