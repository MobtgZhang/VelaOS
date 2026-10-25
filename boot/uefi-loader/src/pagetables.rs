//! Bootstrap page tables, built before `ExitBootServices`.
//!
//! # Why the loader has to build these
//!
//! OVMF enters the loader in long mode with paging already enabled, and the
//! firmware's tables identity-map low RAM only. The kernel, however, is
//! *linked* at `0xFFFF_8000_0000_0000` — the higher half. When the loader
//! jumps to the kernel's entry point it jumps to an address the firmware's
//! tables do not map, so the very first instruction fetch raises a page fault
//! at `CR2 = 0xFFFF800000001970` and the machine dies before the kernel has
//! executed a single byte.
//!
//! The fix has to happen *before* the kernel runs, because the kernel's own
//! page-table builder needs a working address space to allocate from. So the
//! loader builds a minimal set of tables while Boot Services still exist:
//!
//! * an **identity map** of usable physical RAM, so the kernel can reach
//!   frames, the framebuffer, the LAPIC and the loader's own pool;
//! * the **kernel image** at its linked virtual base, so the jump lands on
//!   mapped memory.
//!
//! # What this is not
//!
//! These tables are a *bridge*, not the kernel's address space. They exist to
//! get control to the kernel at its linked address. The kernel builds its own
//! tables proper as soon as it can allocate, and switches to them.
//!
//! # Physical memory is written directly
//!
//! The tables live in the loader's pool, and the pool is *identity-mapped* by
//! the firmware at the time the loader runs. So a pointer into the pool is a
//! pointer to the physical frame it occupies — which is what makes
//! "write the PML4's physical address into `CR3`" a one-line operation rather
//! than a translation problem.
//!
//! # The physical memory types
//!
//! The memory types used here are the **UEFI** ones, as the firmware
//! reports them: 1 = loader code, 2 = loader data, 3 = boot services code,
//! 4 = boot services data, 7 = conventional. Type 7 is the important one —
//! it is ordinary RAM, and it is most of the machine.

use alloc::vec::Vec;

use uefi::mem::memory_map::{MemoryMap, MemoryMapOwned};

use crate::pool::Pool;

/// Bytes per page.
const PAGE_SIZE: u64 = 4096;

/// Entries in one page-table level.
const ENTRIES: u64 = 512;

/// Bytes covered by one page-table entry at each level.
mod span {
    /// A page-table entry covers 4 KiB.
    pub const PAGE_TABLE: u64 = 4096;
    /// A page-directory entry covers 2 MiB.
    pub const PAGE_DIRECTORY: u64 = 2 * 1024 * 1024;
}

/// Page-table entry flags.
mod flags {
    /// The entry is valid.
    pub const PRESENT: u64 = 1 << 0;
    /// The entry is writable.
    pub const WRITABLE: u64 = 1 << 1;
    /// Cache disabled. Used for device memory.
    pub const NO_CACHE: u64 = 1 << 4;
    /// Large page. Only meaningful at the page-directory level.
    pub const PSE: u64 = 1 << 7;
    /// Available to software; the hardware ignores it.
    ///
    /// Used as this crate's marker for "this entry points at a lower table",
    /// exactly as `vela-hal-x86-64`'s `paging` module does, so the two
    /// implementations agree on what a table pointer looks like.
    pub const AVAILABLE: u64 = 1 << 9;
    /// No-execute.
    pub const NX: u64 = 1 << 63;

    /// The bits of an entry that describe an address rather than a property.
    pub const ADDRESS_MASK: u64 = 0x000F_FFFF_FFFF_F000;
}

/// The physical address of the local APIC.
///
/// The kernel's timer reads and writes it directly, so it must be mapped.
pub const LAPIC_BASE: u64 = 0xFEE0_0000;

/// A region of physical memory, as the builder needs it.
#[derive(Debug, Clone, Copy)]
pub struct Region {
    /// First physical address.
    pub start: u64,
    /// One past the last physical address.
    pub end: u64,
}

/// One piece of the kernel image, with the permissions it should have.
///
/// # Why the image is described in pieces
///
/// A `PT_LOAD` segment in a VelaOS kernel covers `.text`, `.rodata` and
/// `.data` together, because the linker script puts them in one output
/// section. The segment's own flags therefore say "writable and executable",
/// which would map the kernel's *code* writable — the opposite of what W^X
/// means. So the loader describes the image by *section* instead, and a
/// section's `SHF_WRITE`/`SHF_EXECINSTR` flags are exactly the permissions it
/// should have.
/// One piece of the kernel image, with the permissions it should have.
///
/// # Why the image is described in two pieces
///
/// A `PT_LOAD` segment in a VelaOS kernel covers `.text`, `.rodata` and
/// `.data` together, because the linker script puts them in one output
/// section. The segment's own flags therefore say "writable and executable",
/// which would map the kernel's *code* writable — the opposite of what W^X
/// means.
///
/// So the image is described as an *executable prefix* and a *data suffix*.
/// The loader computes the boundary from the ELF: the end of the last
/// segment that has a file extent. Everything from there on — `.data`,
/// `.bss` — is data. That gives the kernel an image whose text is read-
/// execute and whose data is read-write, which is the split W^X needs.
pub struct ImageRegion {
    /// The image's lowest virtual address.
    pub vaddr: u64,
    /// The physical address the image's first byte was copied to.
    ///
    /// The loader relocates the image, so this is *not* the ELF's `p_paddr`:
    /// it is the pool address the image's bytes actually occupy. Every byte
    /// keeps its offset from the image's lowest virtual address, so a
    /// virtual-to-physical translation is a single subtraction.
    pub paddr: u64,
    /// The image's total length in bytes.
    pub len: u64,
    /// The length of the image's executable prefix, in bytes.
    ///
    /// `[vaddr, vaddr + text_bytes)` is mapped read-execute; the rest is
    /// mapped read-write.
    pub text_bytes: u64,
}

/// A bootstrap address space: the PML4 plus the tables it points at.
///
/// The tables are ordinary memory in the loader's pool. Because the pool is
/// identity-mapped by the firmware, the address of a table *is* its physical
/// address, and the PML4's address can be handed straight to `CR3`.
pub struct BootstrapTables {
    /// The physical address of the PML4.
    pml4_paddr: u64,
    /// The number of pool pages the tables consumed.
    pages: u64,
}

impl BootstrapTables {
    /// The physical address of the PML4, for `CR3`.
    pub const fn pml4_paddr(&self) -> u64 {
        self.pml4_paddr
    }

    /// How many pool pages the tables took.
    pub const fn pages(&self) -> u64 {
        self.pages
    }
}

/// One page-table level: 512 eight-byte entries, one page.
#[repr(C, align(4096))]
struct Table([u64; ENTRIES as usize]);

/// The state of the tree being built.
///
/// It holds only *addresses*, never references, so the pool can be borrowed
/// mutably while the tree is being written — the tables are reached through
/// raw physical addresses, which is what the CPU does with them anyway.
struct Tree {
    /// The physical address of the PML4.
    pml4_paddr: u64,
    /// Pool pages consumed so far.
    pages: u64,
}

/// Take a fresh, zeroed page from the pool for use as a page table.
///
/// # Panics
///
/// Panics when the pool is exhausted. The loader sizes the pool from
/// [`pages_needed`] and the memory map, so a shortfall means the sizing
/// arithmetic is wrong — and a loud panic at boot is far better than a page
/// table that silently omits a region.
fn alloc_table(pool: &mut Pool, tree: &mut Tree) -> u64 {
    let paddr = pool
        .alloc_pages(PAGE_SIZE, PAGE_SIZE)
        .expect("bootstrap page tables exhausted the boot pool");
    tree.pages += 1;
    // The pool memory is not zeroed by UEFI, so the table is zeroed here.
    //
    // SAFETY: `paddr` is a page the pool owns exclusively, and the pool is
    // identity-mapped by the firmware, so this writes the physical frame the
    // table will live in.
    unsafe {
        core::ptr::write_bytes(paddr as *mut u8, 0, PAGE_SIZE as usize);
    }
    paddr
}

/// The address of entry `index` in the table at `table_paddr`.
fn entry_ptr(table_paddr: u64, index: u64) -> *mut u64 {
    // SAFETY: `table_paddr` names a page-table frame and `index` is a 9-bit
    // field taken from a canonical address, so the result is inside the frame.
    unsafe { (table_paddr as *mut u64).add(index as usize) }
}

/// Read entry `index` of the table at `table_paddr`.
fn read_entry(table_paddr: u64, index: u64) -> u64 {
    // SAFETY: as [`entry_ptr`].
    unsafe { core::ptr::read_volatile(entry_ptr(table_paddr, index)) }
}

/// Write entry `index` of the table at `table_paddr`.
fn write_entry(table_paddr: u64, index: u64, value: u64) {
    // SAFETY: as [`entry_ptr`].
    unsafe { core::ptr::write_volatile(entry_ptr(table_paddr, index), value) }
}

/// A page-table entry that points at a lower table.
fn table_pointer(paddr: u64) -> u64 {
    paddr | flags::PRESENT | flags::WRITABLE | flags::AVAILABLE
}

/// A 4 KiB leaf entry for `pa`.
///
/// # The `executable` argument is not optional
///
/// The identity map the loader builds covers **its own code**: the loader is
/// still running when it writes the new PML4 into `CR3`, and the instructions
/// that follow that write — `mov rsp, ...` and `jmp entry` — are fetched from
/// a page the identity map covers. A map that marks every page `NX` therefore
/// kills the loader on the *first instruction fetch after the CR3 write*,
/// with a `#PF` whose error code says "instruction fetch" and whose faulting
/// address is inside the loader's own image. That is a far more confusing
/// failure than "the kernel did not start", because the kernel's entry
/// address is nowhere in the fault report.
///
/// The identity map is therefore executable. The loader's own tables are a
/// *bridge*: the kernel installs its own address space, with proper W^X, as
/// soon as it can allocate.
fn leaf_4kib(pa: u64, writable: bool, executable: bool, uncacheable: bool) -> u64 {
    let mut entry = pa & flags::ADDRESS_MASK;
    entry |= flags::PRESENT;
    if writable {
        entry |= flags::WRITABLE;
    }
    if uncacheable {
        entry |= flags::NO_CACHE;
    }
    if !executable {
        entry |= flags::NX;
    }
    entry
}

/// A 2 MiB leaf entry for `pa`.
///
/// # Why this is executable too
///
/// See [`leaf_4kib`]: the loader's own code is inside the identity map, and
/// the loader is still executing when the new `CR3` takes effect.
fn leaf_2mib(pa: u64, writable: bool, uncacheable: bool) -> u64 {
    let mut entry = pa & flags::ADDRESS_MASK;
    entry |= flags::PRESENT | flags::PSE;
    if writable {
        entry |= flags::WRITABLE;
    }
    if uncacheable {
        entry |= flags::NO_CACHE;
    }
    entry
}

/// The physical address of the page table holding `va`'s leaf entry,
/// creating the intermediate tables as needed.
///
/// # Panics
///
/// Panics when the pool is exhausted.
fn page_table_for(pool: &mut Pool, tree: &mut Tree, va: u64) -> u64 {
    let pml4_index = (va >> 39) & 0x1FF;
    let pdpt_index = (va >> 30) & 0x1FF;
    let pd_index = (va >> 21) & 0x1FF;

    // --- PML4 -> PDPT ---
    let existing = read_entry(tree.pml4_paddr, pml4_index);
    let pdpt_paddr = if existing & flags::PRESENT != 0 {
        existing & flags::ADDRESS_MASK
    } else {
        let paddr = alloc_table(pool, tree);
        write_entry(tree.pml4_paddr, pml4_index, table_pointer(paddr));
        paddr
    };

    // --- PDPT -> page directory ---
    let existing = read_entry(pdpt_paddr, pdpt_index);
    let pd_paddr = if existing & flags::PRESENT != 0 {
        existing & flags::ADDRESS_MASK
    } else {
        let paddr = alloc_table(pool, tree);
        write_entry(pdpt_paddr, pdpt_index, table_pointer(paddr));
        paddr
    };

    // --- page directory -> page table ---
    let existing = read_entry(pd_paddr, pd_index);
    let pt_paddr = if existing & flags::PRESENT != 0 {
        existing & flags::ADDRESS_MASK
    } else {
        let paddr = alloc_table(pool, tree);
        write_entry(pd_paddr, pd_index, table_pointer(paddr));
        paddr
    };

    pt_paddr
}

/// Map one 4 KiB page of physical memory at its own physical address.
///
/// # `executable` matters, and it is not always true
///
/// The *identity* map of ordinary RAM must be executable, because the loader
/// is still executing its own code out of it when the new `CR3` takes effect.
/// Device memory — the LAPIC and the framebuffer — is mapped
/// non-executable, because there is nothing to execute there and a mapping
/// that permits instruction fetch from device memory is a hole in W^X.
///
/// # Panics
///
/// Panics when the pool is exhausted.
fn map_4kib(pool: &mut Pool, tree: &mut Tree, pa: u64, uncacheable: bool, executable: bool) {
    let pt_paddr = page_table_for(pool, tree, pa);
    let index = (pa >> 12) & 0x1FF;
    write_entry(
        pt_paddr,
        index,
        leaf_4kib(pa, true, executable, uncacheable),
    );
}

/// Map one 2 MiB page of physical memory at its own physical address.
///
/// A page-directory slot that already holds a *page table* is left alone: a
/// 4 KiB mapping and a 2 MiB mapping cannot coexist in one slot, and the
/// 4 KiB mappings are the more precise ones.
///
/// # Panics
///
/// Panics when the pool is exhausted.
fn map_2mib(pool: &mut Pool, tree: &mut Tree, pa: u64, uncacheable: bool) {
    // The three indices of the walk, in the order the CPU takes them.
    //
    // * The **PML4** entry is selected by bits 47..39.
    // * The **PDPT** entry is selected by bits 38..30.
    // * The **page-directory** entry is selected by bits 29..21.
    //
    // Getting the PML4 and PDPT levels right matters on a machine with more
    // than 512 GiB, where the two indices differ. The PDPT level is the one
    // that bites on *every* machine, because the PDPT and the page directory
    // have different index fields.
    let pml4_index = (pa >> 39) & 0x1FF;
    let pdpt_index = (pa >> 30) & 0x1FF;
    let pd_index = (pa >> 21) & 0x1FF;

    // --- PML4 -> PDPT ---
    let existing = read_entry(tree.pml4_paddr, pml4_index);
    let pdpt_paddr = if existing & flags::PRESENT != 0 {
        existing & flags::ADDRESS_MASK
    } else {
        let paddr = alloc_table(pool, tree);
        write_entry(tree.pml4_paddr, pml4_index, table_pointer(paddr));
        paddr
    };

    // --- PDPT -> page directory ---
    //
    // The PDPT entry is selected by the **PDPT index**, not by the
    // page-directory index.
    //
    // This is subtle and catastrophic in a way that is easy to miss: the two
    // index fields come from different bits of the address, so using the
    // page-directory index here selects the wrong gigabyte's page directory.
    // Every 2 MiB page in the machine would then be written into a page
    // directory the CPU never consults for it — and because the *page
    // directory* slot is indexed correctly, the map looks complete: the
    // entries exist, they are present, and they point at the right physical
    // addresses. The CPU simply never reaches them.
    //
    // The symptom is an identity map that covers the low megabytes (which
    // are mapped 4 KiB at a time, through a different code path) and then
    // *nothing* above them, so the kernel faults on the first access to
    // almost every physical frame — with a `#PF` whose `CR2` is an ordinary
    // physical address and no obvious cause. The table count also collapses,
    // which makes the symptom look like a pool-sizing bug rather than an
    // index bug.
    let existing = read_entry(pdpt_paddr, pdpt_index);
    let pd_paddr = if existing & flags::PRESENT != 0 {
        existing & flags::ADDRESS_MASK
    } else {
        let paddr = alloc_table(pool, tree);
        write_entry(pdpt_paddr, pdpt_index, table_pointer(paddr));
        paddr
    };

    // A slot that already holds a page table keeps it.
    if read_entry(pd_paddr, pd_index) & flags::AVAILABLE != 0 {
        return;
    }

    write_entry(pd_paddr, pd_index, leaf_2mib(pa, true, uncacheable));
}

/// Collect the usable RAM regions from a firmware memory map.
fn usable_regions(memory_map: &MemoryMapOwned) -> Vec<Region> {
    let meta = memory_map.meta();
    let desc_size = meta.desc_size as usize;
    let raw = memory_map.buffer();

    let mut regions = Vec::new();
    let mut offset = 0usize;
    while offset + desc_size <= raw.len() {
        let kind = u32::from_le_bytes([
            raw[offset],
            raw[offset + 1],
            raw[offset + 2],
            raw[offset + 3],
        ]);
        let physical_start = u64::from_le_bytes([
            raw[offset + 8],
            raw[offset + 9],
            raw[offset + 10],
            raw[offset + 11],
            raw[offset + 12],
            raw[offset + 13],
            raw[offset + 14],
            raw[offset + 15],
        ]);
        let page_count = u64::from_le_bytes([
            raw[offset + 24],
            raw[offset + 25],
            raw[offset + 26],
            raw[offset + 27],
            raw[offset + 28],
            raw[offset + 29],
            raw[offset + 30],
            raw[offset + 31],
        ]);
        if is_usable_ram(kind) {
            let end = physical_start.saturating_add(page_count.saturating_mul(PAGE_SIZE));
            if end > physical_start {
                regions.push(Region {
                    start: physical_start,
                    end,
                });
            }
        }
        offset += desc_size;
    }
    regions
}

/// True when a firmware memory type is ordinary RAM the kernel may use.
///
/// The UEFI types: 1 = loader code, 2 = loader data, 3 = boot services code,
/// 4 = boot services data, 7 = conventional. Everything else is
/// firmware-reserved or device memory.
///
/// Type 7 is the one that matters: it is the machine's actual RAM, and it is
/// most of it. The loader's own code and data are included because the loader
/// is *running* out of them when the identity map is built, and a page-table
/// walk that faulted on the loader's own instructions would be fatal.
fn is_usable_ram(kind: u32) -> bool {
    matches!(kind, 1 | 2 | 3 | 4 | 7)
}

/// Build the bootstrap page tables.
///
/// The tables map:
///
/// * usable physical RAM, identity-mapped with 2 MiB pages;
/// * the local APIC window at [`LAPIC_BASE`], identity-mapped and uncached;
/// * the framebuffer, identity-mapped and uncached;
/// * the kernel image, at its linked virtual addresses, with 4 KiB pages and
///   the permissions its sections ask for.
///
/// # Panics
///
/// Panics when the pool cannot hold the tables. The loader sizes the pool
/// from the memory map and [`pages_needed`], so a shortfall is a build-time
/// sizing bug rather than a runtime condition.
pub fn build(
    pool: &mut Pool,
    memory_map: &MemoryMapOwned,
    image: &ImageRegion,
    framebuffer_paddr: u64,
    framebuffer_bytes: u64,
) -> BootstrapTables {
    let pml4_paddr = pool
        .alloc_pages(PAGE_SIZE, PAGE_SIZE)
        .expect("bootstrap page tables exhausted the boot pool");
    // SAFETY: `pml4_paddr` is a page the pool owns exclusively, and the pool
    // is identity-mapped by the firmware, so this zeroes the physical frame
    // the PML4 will live in.
    unsafe {
        core::ptr::write_bytes(pml4_paddr as *mut u8, 0, PAGE_SIZE as usize);
    }
    let mut tree = Tree {
        pml4_paddr,
        pages: 1,
    };

    let regions = usable_regions(memory_map);

    // The boot pool is mapped explicitly, and it has to be.
    //
    // The pool is allocated with `AllocatePages` *after* the firmware's
    // memory map was read, so the map does not describe it — the map is a
    // snapshot of the firmware's state before the loader allocated anything.
    // The pool holds the kernel image, both stacks, the kernel's heap, the
    // bootstrap page tables and the `VelaBootInfo` page, and the loader is
    // still running out of pool memory (its stack is in the pool) when the
    // new `CR3` takes effect.
    //
    // So without this mapping the loader faults on the first write after
    // `mov cr3` — a write fault, not an instruction fetch, which is why it
    // looks like a different bug from the `NX` one and is easy to
    // misdiagnose.
    //
    // The pool's extent is known exactly: it is one contiguous reservation.
    let pool_range = Region {
        start: pool.base(),
        end: pool.base().saturating_add(pool.len()),
    };

    // The framebuffer's extent, so the identity map can leave it alone. It is
    // device memory: mapping it as ordinary cacheable RAM would mean the
    // CPU's write buffers held pixels that might never reach the screen.
    let fb_range = if framebuffer_paddr != 0 && framebuffer_bytes != 0 {
        Some(Region {
            start: framebuffer_paddr,
            end: framebuffer_paddr.saturating_add(framebuffer_bytes),
        })
    } else {
        None
    };

    // --- Pass one: the 4 KiB pages ---
    //
    // These come first because a 2 MiB page and a 4 KiB page cannot share a
    // page-directory slot. Mapping the precise pages first and the large
    // ones afterwards — skipping any slot that already holds a page table —
    // means no large page ever displaces a precise one.
    for region in &regions {
        let large_start = align_up(region.start, span::PAGE_DIRECTORY);
        let large_end = align_down(region.end, span::PAGE_DIRECTORY);
        // The head of the region.
        let mut pa = region.start;
        while pa < large_start.min(region.end) {
            if !overlaps(fb_range, pa) {
                // Executable: this is ordinary RAM, and the loader's own code
                // lives in a `LOADER_CODE` region that is part of this map.
                map_4kib(pool, &mut tree, pa, false, true);
            }
            pa += PAGE_SIZE;
        }
        // The tail of the region.
        let mut pa = large_end;
        while pa < region.end {
            if !overlaps(fb_range, pa) {
                map_4kib(pool, &mut tree, pa, false, true);
            }
            pa += PAGE_SIZE;
        }
    }

    // The boot pool, mapped page by page.
    //
    // It is not in the firmware's memory map — see the note where
    // `pool_range` is built — and it holds the loader's own stack, so the
    // loader is writing into it immediately after `mov cr3`.
    //
    // The pages are mapped 4 KiB at a time rather than as 2 MiB pages
    // because the pool is a few megabytes at an arbitrary alignment: it needs
    // at most a head page, a body and a tail page, and the 4 KiB form keeps
    // the mapping exact.
    {
        let mut pa = align_down(pool_range.start, PAGE_SIZE);
        while pa < pool_range.end {
            map_4kib(pool, &mut tree, pa, false, true);
            pa += PAGE_SIZE;
        }
    }

    // The local APIC. It is device memory, so it is uncached — and there is
    // nothing to execute there, so it is not executable.
    map_4kib(pool, &mut tree, LAPIC_BASE, true, false);

    // The framebuffer is MMIO: it must be uncached, or the CPU's
    // write-combining buffers will hold pixels that never reach the screen.
    //
    // It is mapped with 4 KiB pages rather than one 2 MiB page, because the
    // aperture is only a few megabytes and the 4 KiB form lets the mapping
    // stop exactly at the end of the framebuffer.
    //
    // It is not executable: it holds pixels, not code.
    if let Some(fb) = fb_range {
        let mut pa = align_down(fb.start, PAGE_SIZE);
        while pa < fb.end {
            map_4kib(pool, &mut tree, pa, true, false);
            pa += PAGE_SIZE;
        }
    }

    // --- Pass two: the 2 MiB pages ---
    for region in &regions {
        let large_start = align_up(region.start, span::PAGE_DIRECTORY);
        let large_end = align_down(region.end, span::PAGE_DIRECTORY);
        let mut pa = large_start;
        while pa < large_end {
            // A large page that would cover the framebuffer is skipped: the
            // framebuffer has its own precise, uncached mapping.
            if !overlaps_large(fb_range, pa) {
                map_2mib(pool, &mut tree, pa, false);
            }
            pa += span::PAGE_DIRECTORY;
        }
    }

    // --- The kernel image ---
    //
    // This is the mapping the whole exercise exists for: the kernel is
    // *linked* in the higher half, and the physical placement the loader
    // chose is unrelated to it. The mapping is made with 4 KiB pages because
    // the image is small and because a large page would need a 2 MiB-aligned
    // physical frame, which the pool does not guarantee.
    //
    // Each region keeps the permissions its ELF section asked for, so the
    // kernel's text is not writable and its data is not executable.
    // --- The kernel image ---
    //
    // This is the mapping the whole exercise exists for: the kernel is
    // *linked* in the higher half, and the physical placement the loader
    // chose is unrelated to it. The mapping is made with 4 KiB pages because
    // the image is small and because a large page would need a 2 MiB-aligned
    // physical frame, which the pool does not guarantee.
    //
    // The image's executable prefix is mapped read-execute and the rest
    // read-write, which is what gives the kernel's own image W^X.
    for region in core::slice::from_ref(image) {
        let mut offset = 0u64;
        while offset < region.len {
            let va = region.vaddr + offset;
            let pa = region.paddr + offset;
            let executable = offset < region.text_bytes;
            let pt_paddr = page_table_for(pool, &mut tree, va);
            let index = (va >> 12) & 0x1FF;
            write_entry(
                pt_paddr,
                index,
                // A page that straddles the boundary is mapped writable: the
                // text is page-aligned in practice, and a page that is
                // partly code and partly data cannot be both.
                leaf_4kib(pa, !executable, executable, false),
            );
            offset += PAGE_SIZE;
        }
    }
    BootstrapTables {
        pml4_paddr,
        pages: tree.pages,
    }
}

/// True when `pa` falls inside `range`.
fn overlaps(range: Option<Region>, pa: u64) -> bool {
    match range {
        Some(r) => pa >= r.start && pa < r.end,
        None => false,
    }
}

/// True when the 2 MiB page at `pa` would cover any part of `range`.
fn overlaps_large(range: Option<Region>, pa: u64) -> bool {
    match range {
        Some(r) => pa < r.end && r.start < pa + span::PAGE_DIRECTORY,
        None => false,
    }
}

/// Round `value` up to a multiple of `align`.
fn align_up(value: u64, align: u64) -> u64 {
    value.div_ceil(align) * align
}

/// Round `value` down to a multiple of `align`.
fn align_down(value: u64, align: u64) -> u64 {
    value - value % align
}

/// The number of pool pages the bootstrap tables need.
///
/// The identity map is the dominant cost: one page table per 2 MiB of
/// identity-mapped RAM, plus one per 4 KiB page at the head and tail of each
/// region. The kernel image adds one page table per 2 MiB of image.
///
/// The count is computed rather than guessed, so the loader can size its pool
/// and a change in the memory map cannot silently overflow it.
pub fn pages_needed(memory_map: &MemoryMapOwned, kernel_bytes: u64) -> u64 {
    let regions = usable_regions(memory_map);

    let mut page_tables = 0u64;
    for region in &regions {
        let large_start = align_up(region.start, span::PAGE_DIRECTORY);
        let large_end = align_down(region.end, span::PAGE_DIRECTORY);
        // One page table per 2 MiB large page.
        page_tables += large_end.saturating_sub(large_start) / span::PAGE_DIRECTORY;
        // The head and the tail are 4 KiB pages, each needing a page table of
        // its own unless they share one.
        let head = large_start.saturating_sub(region.start) / PAGE_SIZE;
        let tail = region.end.saturating_sub(large_end) / PAGE_SIZE;
        page_tables += head.div_ceil(ENTRIES) + tail.div_ceil(ENTRIES);
    }

    // The intermediate levels: one page directory per 512 page tables, and
    // one PDPT for them. The PML4 is one page.
    let directories = page_tables.div_ceil(ENTRIES).max(1);
    let pdpts = directories.div_ceil(ENTRIES).max(1);

    // The kernel image: one page table per 2 MiB of image.
    let kernel_tables = kernel_bytes.div_ceil(span::PAGE_DIRECTORY).max(1);

    // A margin, so that a firmware whose memory map is slightly different
    // from the one the pool was sized against cannot exhaust the pool.
    1 + page_tables + directories + pdpts + kernel_tables + 16
}

/// The size of a page table in bytes.
///
/// A compile-time check that the table type is exactly one page: a table
/// that was not page-sized would be mapped wrongly, and the failure would
/// show up as a page fault rather than as a link error.
const _: () = assert!(core::mem::size_of::<Table>() == PAGE_SIZE as usize);
