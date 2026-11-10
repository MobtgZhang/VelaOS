//! Address spaces on AArch64.
//!
//! # What an AArch64 address space is
//!
//! The hardware walks a four-level table of 4 KiB pages. The kernel's
//! translation regime puts the user half in `TTBR0_EL1` and the kernel half
//! in `TTBR1_EL1`, with `TCR_EL1` saying how much of the address space each
//! covers — sixteen bits each on a 48-bit address space, so each half is
//! 256 TiB.
//!
//! The tables themselves are ordinary memory, walked by the kernel — which is
//! what [`crate::paging`] does — and the root is named by `TTBR0_EL1` (or
//! `TTBR1_EL1` for the kernel half).
//!
//! # What this module owns
//!
//! The pool of table pages, the space itself, and the mapping operations the
//! kernel's memory manager drives.

use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use spin::Mutex;

use vela_arch_common::{AddressSpaceId, MapError, PageFlags, PAGE_SIZE};

use crate::paging::{self, PhysBackend};

/// The first physical address the table pool hands out.
///
/// The pool is a bump allocator: the boot code reserves the region below
/// this address for tables and hands the rest to the frame allocator.
pub const DEFAULT_POOL_BASE: u64 = 0x0010_0000;

/// How to find the physical address of a virtual one.
pub trait PhysOfVirt {
    /// The physical address of `virt`.
    fn phys_of(&self, virt: *const u8) -> u64;
}

/// The identity mapping: the physical address *is* the virtual address.
///
/// It is right for the boot-time window, where the kernel runs with the
/// identity mapping the boot code installs.
#[derive(Debug, Clone, Copy, Default)]
pub struct IdentityPhys;

impl PhysOfVirt for IdentityPhys {
    fn phys_of(&self, virt: *const u8) -> u64 {
        virt as u64
    }
}

/// A source of physical frames.
///
/// The kernel's frame allocator implements this; the address space uses it
/// for the frames it maps, not for the table pages, which come from the
/// pool.
pub trait FrameSource {
    /// Allocate one frame, returning its physical address.
    fn alloc_frame(&mut self) -> Result<u64, MapError>;
    /// Release a frame.
    ///
    /// # Safety
    ///
    /// The frame must have come from [`FrameSource::alloc_frame`] and must
    /// not be mapped anywhere.
    unsafe fn free_frame(&mut self, pa: u64);
}

/// The pool of page-table pages.
///
/// The pool owns a contiguous region of *physical memory* — the tables
/// themselves — and hands out pages from it. It is a bump allocator: a table
/// is never returned to the frame allocator, because a table freed while
/// something still points at it is a use-after-free in the page-table walk,
/// and holding a table for the life of the address space costs a few
/// kilobytes.
///
/// The memory is modelled as a byte image rather than reached through a
/// pointer, so that the same code runs on the host — where there is no
/// physical memory to point at — and on the target.
#[derive(Debug)]
pub struct TablePool {
    /// The physical memory the pool owns.
    memory: Vec<u8>,
    /// The physical address of the first byte of `memory`.
    base: u64,
    /// The next physical address to hand out.
    next_table: u64,
    /// The tables handed out so far.
    tables: Vec<u64>,
}

impl TablePool {
    /// An empty pool, handing out table frames from `first_table` upward.
    pub fn new(first_table: u64) -> Self {
        Self {
            memory: Vec::new(),
            base: first_table,
            next_table: first_table,
            tables: Vec::new(),
        }
    }

    /// The number of tables the pool holds.
    pub fn table_count(&self) -> usize {
        self.tables.len()
    }

    /// The offset in the pool's memory that `pa` names.
    fn offset_of(&self, pa: u64) -> Option<usize> {
        pa.checked_sub(self.base).map(|offset| offset as usize)
    }
}

impl PhysBackend for TablePool {
    fn read_pte(&mut self, pa: u64) -> Result<u64, MapError> {
        let Some(offset) = self.offset_of(pa) else {
            // An address outside the pool is an address the walk has no
            // business reading; the honest answer is "no entry".
            return Err(MapError::NotMapped);
        };
        if offset + 8 > self.memory.len() {
            return Err(MapError::NotMapped);
        }
        let mut bytes = [0u8; 8];
        bytes.copy_from_slice(&self.memory[offset..offset + 8]);
        Ok(u64::from_le_bytes(bytes))
    }

    fn write_pte(&mut self, pa: u64, value: u64) -> Result<(), MapError> {
        let Some(offset) = self.offset_of(pa) else {
            return Err(MapError::NotMapped);
        };
        if offset + 8 > self.memory.len() {
            return Err(MapError::NotMapped);
        }
        self.memory[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
        Ok(())
    }

    fn alloc_table(&mut self) -> Result<u64, MapError> {
        let pa = self.next_table;
        self.next_table += PAGE_SIZE;
        let end = (self.next_table - self.base) as usize;
        self.memory.resize(end, 0);
        self.tables.push(pa);
        Ok(pa)
    }
}

/// An address space.
pub struct AddressSpace {
    /// The space's identity.
    id: AddressSpaceId,
    /// The pool the tables come from.
    pool: Mutex<TablePool>,
    /// The root table's physical address.
    root_pa: AtomicU64,
    /// The number of leaf mappings.
    mapped_frames: AtomicUsize,
}

impl core::fmt::Debug for AddressSpace {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("AddressSpace")
            .field("id", &self.id)
            .field("root_pa", &self.root_pa.load(Ordering::Acquire))
            .field("mapped_frames", &self.mapped_frames.load(Ordering::Acquire))
            .finish()
    }
}

impl AddressSpace {
    /// A fresh space with an empty pool.
    pub fn new(id: AddressSpaceId) -> Self {
        Self {
            id,
            pool: Mutex::new(TablePool::new(DEFAULT_POOL_BASE)),
            root_pa: AtomicU64::new(0),
            mapped_frames: AtomicUsize::new(0),
        }
    }

    /// The space's identity.
    pub const fn id(&self) -> AddressSpaceId {
        self.id
    }

    /// The root table's physical address.
    pub fn root_pa(&self) -> u64 {
        self.root_pa.load(Ordering::Acquire)
    }

    /// The root table's physical address, under the name the x86-64 crate
    /// gives the same value.
    pub fn pml4_pa(&self) -> u64 {
        self.root_pa()
    }

    /// The root table as a pointer, for the identity-mapped boot window.
    pub fn pml4_ptr(&self) -> *const u8 {
        self.root_pa() as *const u8
    }

    /// The number of leaf mappings.
    pub fn mapped_frames(&self) -> usize {
        self.mapped_frames.load(Ordering::Acquire)
    }

    /// True when `va` is mapped.
    pub fn is_mapped(&self, va: u64) -> bool {
        self.translate(va).is_some()
    }

    /// Map one page.
    pub fn map_page(&self, va: u64, pa: u64, flags: PageFlags) -> Result<(), MapError> {
        if !va.is_multiple_of(PAGE_SIZE) {
            return Err(MapError::MisalignedVirtual);
        }
        if !pa.is_multiple_of(PAGE_SIZE) {
            return Err(MapError::MisalignedPhysical);
        }
        let mut pool = self.pool.lock();
        paging::map_page(&mut *pool, self.root_pa(), va, pa, flags)?;
        self.mapped_frames.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    /// Map a range of pages.
    pub fn map_range(
        &self,
        va: u64,
        pa: u64,
        len: usize,
        flags: PageFlags,
    ) -> Result<(), MapError> {
        if len == 0 || !(len as u64).is_multiple_of(PAGE_SIZE) {
            return Err(MapError::MisalignedVirtual);
        }
        for i in 0..(len as u64 / PAGE_SIZE) {
            let step = i * PAGE_SIZE;
            self.map_page(va + step, pa + step, flags)?;
        }
        Ok(())
    }

    /// Unmap one page, returning the frame it named.
    pub fn unmap_page(&self, va: u64) -> Result<u64, MapError> {
        let mut pool = self.pool.lock();
        let pa = paging::unmap_page(&mut *pool, self.root_pa(), va)?;
        self.mapped_frames.fetch_sub(1, Ordering::Relaxed);
        Ok(pa)
    }

    /// Unmap a range, returning the frames it named.
    pub fn unmap_range(&self, va: u64, len: usize) -> Result<Vec<u64>, MapError> {
        if len == 0 || !(len as u64).is_multiple_of(PAGE_SIZE) {
            return Err(MapError::MisalignedVirtual);
        }
        let mut frames = Vec::new();
        for i in 0..(len as u64 / PAGE_SIZE) {
            let step = i * PAGE_SIZE;
            frames.push(self.unmap_page(va + step)?);
        }
        Ok(frames)
    }

    /// Change a mapping's flags.
    pub fn set_flags(&self, va: u64, flags: PageFlags) -> Result<(), MapError> {
        let mut pool = self.pool.lock();
        paging::set_flags(&mut *pool, self.root_pa(), va, flags)
    }

    /// The physical address `va` maps to.
    pub fn translate(&self, va: u64) -> Option<u64> {
        let mut pool = self.pool.lock();
        // A walk that cannot be completed means the address is not
        // mapped; the caller asked a yes/no question.
        paging::translate(&mut *pool, self.root_pa(), va).unwrap_or_default()
    }

    /// A short human-readable dump, for the boot banner.
    pub fn summary(&self) -> Vec<String> {
        vec![alloc::format!(
            "root {:#014x}, {} leaf mapping(s)",
            self.root_pa(),
            self.mapped_frames()
        )]
    }
}

/// Activate an address space on this hart.
///
/// AArch64 names the user-half root through `TTBR0_EL1` and the kernel-half
/// root through `TTBR1_EL1`, and the TLB has to be told about the change:
/// the entries it holds describe the *old* space.
///
/// # Safety
///
/// The space's tables must be live and mapped, and the caller must be
/// willing to have this hart's translations replaced.
pub unsafe fn activate(space: &AddressSpace) {
    if !crate::regs::ON_BARE_METAL {
        return;
    }
    // SAFETY: the caller guarantees the space's tables are live.
    unsafe {
        crate::regs::write_ttbr0_el1(space.root_pa());
        crate::regs::write_ttbr1_el1(space.root_pa());
        crate::regs::tlbi_vmalle1();
        crate::regs::dsb();
        crate::regs::isb();
    }
}

#[cfg(test)]
extern crate std;

#[cfg(test)]
mod tests {
    use super::*;

    /// A frame source that hands out consecutive frames.
    struct FakeFrames {
        next: u64,
        budget: u64,
    }

    impl FakeFrames {
        fn new(budget: u64) -> Self {
            Self {
                next: 0x0020_0000,
                budget,
            }
        }
    }

    impl FrameSource for FakeFrames {
        fn alloc_frame(&mut self) -> Result<u64, MapError> {
            if self.budget == 0 {
                return Err(MapError::OutOfMemory);
            }
            self.budget -= 1;
            let frame = self.next;
            self.next += PAGE_SIZE;
            Ok(frame)
        }

        unsafe fn free_frame(&mut self, _pa: u64) {
            self.budget += 1;
        }
    }

    #[test]
    fn a_fresh_space_has_no_mappings() {
        let space = AddressSpace::new(AddressSpaceId(0x1000));
        assert_eq!(space.mapped_frames(), 0);
        assert!(!space.is_mapped(0x1000));
        assert_eq!(space.id().0, 0x1000);
    }

    #[test]
    fn the_identity_mapping_is_the_identity() {
        let phys = IdentityPhys;
        let address = 0x1234u64 as *const u8;
        assert_eq!(phys.phys_of(address), 0x1234);
    }

    #[test]
    fn a_frame_source_hands_out_consecutive_frames() {
        let mut frames = FakeFrames::new(2);
        let first = frames.alloc_frame().expect("a frame");
        let second = frames.alloc_frame().expect("a frame");
        assert_eq!(second - first, PAGE_SIZE);
        assert_eq!(frames.alloc_frame().unwrap_err(), MapError::OutOfMemory);
    }

    #[test]
    fn the_pool_hands_out_aligned_tables() {
        let mut pool = TablePool::new(DEFAULT_POOL_BASE);
        let first = pool.alloc_table().expect("a table");
        let second = pool.alloc_table().expect("a table");
        assert!(first.is_multiple_of(PAGE_SIZE));
        assert_eq!(second - first, PAGE_SIZE);
        assert_eq!(pool.table_count(), 2);
    }
}
