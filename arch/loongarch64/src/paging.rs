//! LA464 four-level page tables, the software TLB walk, and `map`/`unmap`.
//!
//! # There is no page-table walker
//!
//! x86-64 walks its page tables in hardware. LoongArch64 does not: address
//! translation is done by an associative TLB (a fully-associative MTLB plus a
//! set-associative software TLB), and *the kernel is responsible for filling
//! TLB entries*. Concretely, to map a page the kernel:
//!
//! 1. Maintains a page-table tree in memory (this module, mirroring the x86
//!    four-level shape),
//! 2. Writes the virtual page number into `CSR.TLBEHI`, the even/odd EntryLo
//!    pair into `CSR.TLBELO0`/`CSR.TLBELO1`, and the page size into
//!    `CSR.TLBIDX.PS`,
//! 3. Issues `tlbwr` (replace the entry indexed by `TLBIDX`) or `tlbfill`
//!    (replace a hardware-chosen entry).
//!
//! On a TLB miss the CPU raises PIL/PIS/PIF (see [`crate::idt`]) and a
//! refill handler repeats steps 2–3 for the faulting address. VelaOS v0
//! programs the TLB eagerly through [`map_page`]/[`set_flags`]/[`unmap_page`]
//! for kernel-owned pages and documents the refill path for user tasks.
//!
//! # TLB entry format
//!
//! A TLB entry is an even/odd pair covering two adjacent pages. The relevant
//! CSRs (LoongArch Reference Manual Vol. 2, Chapter 4):
//!
//! * `TLBIDX`: `INDEX` bits 11:0 (target entry), `PS` bits 30:25 (page size
//!   exponent), `NE` bit 31 (issue `tlbwr` with `NE` set to *invalidate*).
//! * `TLBEHI`: `VPPN` bits 47:13 — the virtual page number ÷ 2 (odd/even).
//! * `TLBELO0/1`: the EntryLo words below, for the even/odd page.
//! * `ASID`: `ASID` bits 9:0, matched against the entry's ASID unless its
//!   `G` bit is set (global entries match every ASID).
//!
//! The EntryLo bit layout — identical for the in-memory PTEs, so a refill is
//! a plain load — is (Linux `arch/loongarch/include/asm/pgtable-bits.h`,
//! `CONFIG_64BIT` branch):
//!
//! ```text
//!  63   62  61  60..48 47...12  11..7        6   5:4   3:2    1   0
//! ┌────┬───┬───┬────────┬─────────┬───────────┬───┬─────┬─────┬───┬───┐
//! │RPLV│ NX│ NR│ (free) │   PPN   │ (software)│ G │ MAT │ PLV │ D │ V │
//! └────┴───┴───┴────────┴─────────┴───────────┴───┴─────┴─────┴───┴───┘
//! ```
//!
//! * `V` (bit 0) — entry is valid. In software-managed mode this *is* the
//!   present bit: QEMU's `pte_present` checks `V` unless the hardware page
//!   walker (`PWCH.HPTW_EN`) is enabled, which VelaOS never enables.
//! * `D` (bit 1) — dirty, which doubles as the *write* enable: a store to an
//!   entry with `D` clear raises PME (EC 4).
//! * `PLV` (bits 3:2) — the privilege levels allowed to access.
//! * `MAT` (bits 5:4) — memory access type: 0 = strong-ordered uncached
//!   (SUC), 1 = coherent cached (CC), 2 = weak-ordered uncached (WUC).
//! * `G` (bit 6) — global: matches every ASID (kernel mappings).
//! * `PPN` (bits 47:12) — physical page number (PALEN ≤ 48 on LA464).
//! * `NR` (bit 61) — no-read (loads raise PNR); `NX` (bit 62) — no-execute
//!   (fetches raise PNX); `RPLV` (bit 63) — restricted PLV.
//! * Software bits 7–11 are ignored by the TLB. VelaOS uses bit 7
//!   ([`sw::PRESENT`]) as the in-memory liveness marker — Linux keeps its
//!   `_PAGE_PRESENT` there too — and bit 9 ([`sw::TABLE`]) to mark interior
//!   table pointers, the same trick the x86 crate uses with an "available"
//!   bit.
//!
//! # The direct mapping windows
//!
//! Kernel text and data live in *direct mapping windows* — fixed VA ranges
//! that the MMU maps to fixed PA ranges with a fixed cache attribute, with
//! no TLB involvement at all. Each of `DMW0`–`DMW3` holds:
//!
//! ```text
//!  63..60      59..48        5:4        3:0
//! ┌────────┬───────────────┬────────┬──────────┐
//! │ VSEG   │ reserved (0)  │  MAT   │ PLV0-3   │
//! │ (VA    │ (PSEG on the  │        │ enable   │
//! │ 63:60) │ 32-bit format)│        │ bits     │
//! └────────┴───────────────┴────────┴──────────┘
//! ```
//!
//! A VA whose top nibble equals a window's `VSEG` maps to `VA[47:0]` (the
//! low 48 bits, i.e. PALEN) if the current `CRMD.PLV` has its enable bit
//! set. Bits 59:48 are reserved on LA64 (the 32-bit DMW format reuses them
//! for `PSEG`, see QEMU's `CSR_DMW_32` fields). Following Linux's convention
//! (`CSR_DMW0_INIT`/`CSR_DMW1_INIT`): VSEG `0x8` = SUC window, VSEG `0x9` =
//! CC window, VSEG `0xA` = WUC window. Because the kernel runs through DMW
//! windows, user-space page tables never need kernel mappings — see
//! [`crate::addrspace`].

use core::fmt;
use vela_arch_common::{MapError, PageFlags, PAGE_SIZE};

/// Log2 of the page size VelaOS uses everywhere: 4 KiB.
///
/// The TLB supports page sizes from 4 KiB (`PS` = 12) up to 1 GiB (`PS` =
/// 30), but VelaOS keeps everything at 4 KiB for parity with the other
/// architectures (and the software TLB's default indexing assumes 16 KiB
/// pages, `CSR.STLBPS`, which is programmed accordingly at boot).
pub const PAGE_SHIFT: u32 = 12;

/// Physical address width: LA464 implements PALEN ≤ 48, and QEMU's model
/// reports exactly 48.
pub const PALEN: u32 = 48;

/// In-memory PTE software bits (bits 7–11 are ignored by the TLB hardware).
pub mod sw {
    /// Set on every live in-memory entry, leaf or table — the walker's
    /// "is this slot occupied" test.
    pub const PRESENT: u64 = 1 << 7;
    /// Set on writable leaves (the software counterpart of EntryLo `D`).
    pub const WRITE: u64 = 1 << 8;
    /// Set on interior table-pointer entries (with `PRESENT`, `V` clear).
    pub const TABLE: u64 = 1 << 9;
    /// Set on reserved-but-unmapped entries (no access, no refill either).
    pub const PROTNONE: u64 = 1 << 10;
}

/// EntryLo architectural bits.
pub mod hw {
    use super::PALEN;

    /// Valid — present in the TLB sense; a refill copies this bit verbatim.
    pub const VALID: u64 = 1 << 0;
    /// Dirty — doubles as the write-enable; a store to a clean entry raises
    /// PME.
    pub const DIRTY: u64 = 1 << 1;
    /// Global — the entry matches every ASID.
    pub const GLOBAL: u64 = 1 << 6;
    /// PPN field: PA bits `[PALEN-1 : 12]`, so bits 47:12 on LA464.
    pub const PPN_MASK: u64 = (1 << PALEN) - (1 << 12);
    /// No-read — loads and fetches raise PNR.
    pub const NO_READ: u64 = 1 << 61;
    /// No-execute — fetches raise PNX.
    pub const NO_EXEC: u64 = 1 << 62;
    /// Restricted PLV — access requires the current PLV to be ≤ the entry's.
    pub const RESTRICTED_PLV: u64 = 1 << 63;
}

/// The `MAT` (memory access type) encodings of EntryLo bits 5:4 and `DMW`
/// bits 5:4.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemAccess {
    /// Strong-ordered uncached — MMIO and never-cached memory.
    StrongUncached,
    /// Coherent cached — normal RAM.
    CoherentCached,
    /// Weak-ordered uncached — write-combining style, rarely used.
    WeakUncached,
}

impl MemAccess {
    /// The two-bit `MAT` encoding.
    pub fn bits(self) -> u64 {
        match self {
            MemAccess::StrongUncached => 0b00,
            MemAccess::CoherentCached => 0b01,
            MemAccess::WeakUncached => 0b10,
        }
    }

    /// Decode a two-bit `MAT` field; 3 is reserved and maps to SUC (VelaOS
    /// never produces it).
    pub fn from_bits(bits: u64) -> MemAccess {
        match bits & 0b11 {
            0b01 => MemAccess::CoherentCached,
            0b10 => MemAccess::WeakUncached,
            _ => MemAccess::StrongUncached,
        }
    }
}

/// The privilege levels of `EntryLo.PLV` / `CRMD.PLV`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Plv {
    /// Kernel (ring 0).
    Kernel,
    /// User (ring 3); rings 1–2 exist but VelaOS does not use them.
    User,
}

impl Plv {
    /// The two-bit encoding (`CRMD.PLV`/`EntryLo.PLV`).
    pub fn bits(self) -> u64 {
        match self {
            Plv::Kernel => 0b00,
            Plv::User => 0b11,
        }
    }
}

/// Build one in-memory PTE for a present leaf page.
///
/// `pa` must be page-aligned. The mapping is *global* only through
/// [`make_leaf_global`]; `MAT` is CC unless `flags.uncacheable` requests SUC;
/// `NX` is clear only when `flags.executable`; `PLV` is ring 3 only when
/// `flags.user`/`user_writable`.
pub fn make_leaf(pa: u64, flags: PageFlags) -> u64 {
    debug_assert_eq!(pa & 0xFFF, 0, "leaf PAs must be page-aligned");
    let mut entry = pa & hw::PPN_MASK;
    entry |= hw::VALID | sw::PRESENT;
    if flags.writable || flags.user_writable {
        entry |= hw::DIRTY | sw::WRITE;
    }
    if flags.user || flags.user_writable {
        entry |= Plv::User.bits() << 2;
    }
    if flags.uncacheable {
        entry |= MemAccess::StrongUncached.bits() << 4;
    } else {
        entry |= MemAccess::CoherentCached.bits() << 4;
    }
    if !flags.executable {
        entry |= hw::NO_EXEC;
    }
    entry
}

/// A global (ASID-independent) variant of a leaf PTE — for kernel mappings
/// that must survive address-space switches.
pub fn make_leaf_global(pa: u64, flags: PageFlags) -> u64 {
    make_leaf(pa, flags) | hw::GLOBAL
}

/// Build an interior table-pointer entry: physical address of the lower
/// table, software `PRESENT` + `TABLE`, and `V` clear so that naïvely filling
/// a TLB entry from this word would leave the page unmapped rather than
/// mis-mapped.
pub fn make_table_pointer(table_pa: u64) -> u64 {
    debug_assert_eq!(table_pa & 0xFFF, 0, "table PAs must be page-aligned");
    (table_pa & hw::PPN_MASK) | sw::PRESENT | sw::TABLE
}

/// Is this in-memory entry a live leaf page?
pub fn is_leaf(entry: u64) -> bool {
    entry & sw::PRESENT != 0 && entry & hw::VALID != 0
}

/// Is this in-memory entry a pointer to a lower table?
pub fn is_table_pointer(entry: u64) -> bool {
    entry & sw::PRESENT != 0 && entry & sw::TABLE != 0 && entry & hw::VALID == 0
}

/// The physical address encoded in a live entry (leaf page or lower table).
pub fn entry_pa(entry: u64) -> u64 {
    entry & hw::PPN_MASK
}

/// The `TLBIDX` word for a `tlbwr` filling the entry at `index` with page
/// size 4 KiB (`PS` = 12) and `NE` clear.
///
/// Format (QEMU `FIELD(CSR_TLBIDX, ...)`, matching the manual): `INDEX` bits
/// 11:0, `PS` bits 29:24, `NE` bit 31.
pub fn tlbidx_value(index: u16) -> u64 {
    debug_assert!(index < 4096, "TLBIDX.INDEX is 12 bits");
    (u64::from(index) & 0xFFF) | (u64::from(PAGE_SHIFT) << 24)
}

/// The `TLBIDX` word with `NE` set: issuing `tlbwr` with it invalidates the
/// entry at `index` instead of filling it.
pub fn tlbidx_invalidate(index: u16) -> u64 {
    tlbidx_value(index) | 1 << 31
}

/// The `TLBEHI` word for a 4 KiB mapping of `va`: `VPPN` = VA[47:13].
pub fn tlbehi_value(va: u64) -> u64 {
    (va >> 13) << 13
}

/// Extract `VPPN` (VA[47:13]) from a virtual address.
pub fn vppn_of(va: u64) -> u64 {
    va >> 13
}

// ---------------------------------------------------------------------------
// Direct mapping windows.
// ---------------------------------------------------------------------------

/// Linux's DMW0 convention: VSEG `0x8`, strong-ordered uncached, PLV 0 only.
pub const DMW_KERNEL_SUC_VSEG: u8 = 0x8;
/// Linux's DMW1 convention: VSEG `0x9`, coherent cached, PLV 0 only.
pub const DMW_KERNEL_CC_VSEG: u8 = 0x9;
/// Linux's DMW2 convention: VSEG `0xA`, weak-ordered uncached.
pub const DMW_WUC_VSEG: u8 = 0xA;

/// Build a `DMW` CSR value for a window over `vseg` (VA bits 63:60) with the
/// given privilege-level enable bits and memory access type.
///
/// `plv_mask` is a bitmask of `CRMD.PLV` values allowed to *use* the window
/// (bit 0 = kernel, bit 3 = user). Bits 59:48 stay zero: on LA64 they are
/// reserved (QEMU's `CSR_DMW_64` defines only `PLV0-3`, `MAT` and `VSEG`;
/// `PSEG` exists only in the 32-bit `CSR_DMW_32` format).
pub fn dmw_value(vseg: u8, plv_mask: u8, mat: MemAccess) -> u64 {
    debug_assert!(vseg <= 0xF);
    debug_assert!(plv_mask <= 0xF);
    u64::from(vseg) << 60 | mat.bits() << 4 | u64::from(plv_mask & 0xF)
}

/// The kernel's SUC window (`DMW0` in Linux's layout): VSEG `0x8`, MMIO-grade
/// uncached, kernel-only.
pub fn dmw_kernel_suc() -> u64 {
    dmw_value(DMW_KERNEL_SUC_VSEG, 0b0001, MemAccess::StrongUncached)
}

/// The kernel's CC window (`DMW1` in Linux's layout): VSEG `0x9`, normal
/// cached RAM, kernel-only.
pub fn dmw_kernel_cc() -> u64 {
    dmw_value(DMW_KERNEL_CC_VSEG, 0b0001, MemAccess::CoherentCached)
}

/// The virtual base of a DMW window: the `VSEG` nibble shifted to bits 63:60.
pub fn dmw_virtual_base(vseg: u8) -> u64 {
    u64::from(vseg) << 60
}

/// Translate a windowed virtual address to physical — pure arithmetic, the
/// exact inverse of what the MMU does for DMW hits (`PA = PSEG<<48 |
/// VA[47:0]`).
pub fn dmw_translate(va: u64, vseg: u8) -> u64 {
    debug_assert_eq!(va >> 60, u64::from(vseg), "VA is outside the window");
    va & ((1 << PALEN) - 1)
}

// ---------------------------------------------------------------------------
// Four-level walk.
//
// LA464 divides the 48-bit VA space as (Linux `pgtable.h`):
//
//   PGD  VA[47:39]   PUD  VA[38:30]   PMD  VA[29:21]   PTE  VA[20:12]
//
// 512 entries per level, each an 8-byte PTE — one 4 KiB table per level.
// ---------------------------------------------------------------------------

/// Entries per level (a 4 KiB table of 8-byte PTEs).
pub const ENTRIES_PER_TABLE: usize = 512;

/// Extract the PGD index (VA bits 47:39).
pub fn pgd_index(va: u64) -> usize {
    ((va >> 39) & 0x1FF) as usize
}

/// Extract the PUD index (VA bits 38:30).
pub fn pud_index(va: u64) -> usize {
    ((va >> 30) & 0x1FF) as usize
}

/// Extract the PMD index (VA bits 29:21).
pub fn pmd_index(va: u64) -> usize {
    ((va >> 21) & 0x1FF) as usize
}

/// Extract the PTE index (VA bits 20:12).
pub fn pte_index(va: u64) -> usize {
    ((va >> 12) & 0x1FF) as usize
}

/// The `CSR.PWCL` word describing the first three walk levels (QEMU
/// `FIELD(CSR_PWCL, ...)`):
///
/// * `PTBASE` bits 4:0 — the shift of the PTE index (12),
/// * `PTWIDTH` bits 9:5 — the width of the PTE index (9),
/// * `DIR1_BASE` bits 14:10 — the shift of the PMD index (21),
/// * `DIR1_WIDTH` bits 19:15 — the width of the PMD index (9),
/// * `DIR2_BASE` bits 24:20 — the shift of the PUD index (30),
/// * `DIR2_WIDTH` bits 29:25 — the width of the PUD index (9),
/// * `PTEWIDTH` bits 31:30 — log2 of the PTE size (3, for 8-byte PTEs).
///
/// VelaOS programs `PWCL`/`PWCH` once at boot — they define the layout the
/// (never enabled) hardware walker and the `lddir`/`ldpte` refill
/// instructions use; the software walk in this module uses the fixed index
/// extractors above, which produce the same values.
pub fn pwcl_value() -> u64 {
    12 | (9 << 5) | (21 << 10) | (9 << 15) | (30 << 20) | (9 << 25) | (3 << 30)
}

/// The `CSR.PWCH` word for the outermost level: `DIR3_BASE` bits 5:0 (PGD
/// index shift 39), `DIR3_WIDTH` bits 11:6 (PGD index width 9), `DIR4_BASE`
/// bits 17:12 / `DIR4_WIDTH` bits 23:18 (unused, no fifth level). `HPTW_EN`
/// (bit 24) stays clear — VelaOS does not use the hardware page walker.
pub fn pwch_value() -> u64 {
    39 | (9 << 6)
}

// ---------------------------------------------------------------------------
// The software walk itself.
// ---------------------------------------------------------------------------

/// A trait for reading and allocating physical memory during walks.
///
/// Walks happen both on bare metal (direct through a DMW window — kernel
/// tables live in the CC window) and on the host (over a fake `Vec`-backed
/// RAM in tests, which is how the walk logic is host-tested).
pub trait PhysBackend {
    /// Read the 8-byte PTE at physical address `pa`.
    ///
    /// # Safety
    ///
    /// `pa` must be the physical address of an allocated 8-byte-aligned
    /// table slot owned by the walker.
    unsafe fn read_phys(&self, pa: u64) -> u64;

    /// Write the 8-byte PTE at physical address `pa`.
    ///
    /// # Safety
    ///
    /// As [`read_phys`].
    unsafe fn write_phys(&mut self, pa: u64, value: u64);

    /// Allocate one zeroed 4 KiB page-table page, returning its PA. Returns
    /// `None` when the frame allocator is exhausted.
    fn alloc_table(&mut self) -> Option<u64>;

    /// Free a table page previously returned by [`alloc_table`].
    ///
    /// # Safety
    ///
    /// `pa` must be a page previously returned by [`alloc_table`] and not
    /// still referenced by any walk.
    unsafe fn free_table(&mut self, pa: u64);
}

/// Errors produced by the walk itself (as opposed to [`MapError`], the
/// kernel-facing surface).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WalkError {
    /// A middle level held a leaf where a table pointer was required (huge
    /// pages are not supported at 4 KiB granularity).
    NotATable,
    /// A slot expected to point at a lower table was empty or malformed.
    BadTablePointer,
    /// The frame allocator ran out of table pages.
    OutOfTables,
}

impl fmt::Display for WalkError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WalkError::NotATable => {
                write!(f, "intermediate entry is a leaf, not a table pointer")
            }
            WalkError::BadTablePointer => {
                write!(f, "intermediate entry is not a table pointer")
            }
            WalkError::OutOfTables => write!(f, "out of page-table pages"),
        }
    }
}

/// The four levels of the LA464 walk, outermost first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    /// `PGD` — VA[47:39].
    Pgd,
    /// `PUD` — VA[38:30].
    Pud,
    /// `PMD` — VA[29:21].
    Pmd,
    /// `PTE` — VA[20:12].
    Pte,
}

impl Level {
    /// The next level down, or `None` at the PTE level.
    pub fn next(self) -> Option<Level> {
        match self {
            Level::Pgd => Some(Level::Pud),
            Level::Pud => Some(Level::Pmd),
            Level::Pmd => Some(Level::Pte),
            Level::Pte => None,
        }
    }
}

/// What one walk step resolved to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WalkStep {
    /// Descend into the table at this physical address.
    Table(u64),
    /// A leaf PTE occupies this physical slot (slot index [`pte_index`]).
    LeafSlot(u64, usize),
}

/// Resolve one descent step: given `level`'s slot value `entry` for `va`,
/// either descend into the table it names, or — at the PTE level — identify
/// the leaf slot. The pure-logic half of every map/unmap/translate.
pub fn walk_step(va: u64, level: Level, entry: u64) -> Result<WalkStep, WalkError> {
    match level {
        Level::Pte => Ok(WalkStep::LeafSlot(entry_pa(entry), pte_index(va))),
        _ => {
            if is_table_pointer(entry) {
                Ok(WalkStep::Table(entry_pa(entry)))
            } else if is_leaf(entry) {
                // A leaf at an intermediate level would be a huge page;
                // VelaOS does not create those (see the module docs).
                Err(WalkError::NotATable)
            } else {
                Err(WalkError::BadTablePointer)
            }
        }
    }
}

/// The slot index within `level`'s table for `va`.
fn index_of(va: u64, level: Level) -> usize {
    match level {
        Level::Pgd => pgd_index(va),
        Level::Pud => pud_index(va),
        Level::Pmd => pmd_index(va),
        Level::Pte => pte_index(va),
    }
}

/// Descend one level from the table at `table_pa`: read the slot indexed for
/// `va` and return the next table's physical address.
///
/// # Safety
///
/// `table_pa` must be an allocated table page.
///
/// # Errors
///
/// [`WalkError::BadTablePointer`] when the slot is empty — callers that
/// expect the full path to exist (translate, unmap) convert this to
/// [`MapError::NotMapped`]; [`ensure_tables`]-style callers allocate instead.
unsafe fn descend<B: PhysBackend>(
    backend: &B,
    table_pa: u64,
    va: u64,
    level: Level,
) -> Result<u64, WalkError> {
    let slot_pa = table_pa + (index_of(va, level) * 8) as u64;
    // SAFETY: slot is 8-byte aligned within an allocated table page.
    let entry = unsafe { backend.read_phys(slot_pa) };
    match walk_step(va, level, entry)? {
        WalkStep::Table(next) => Ok(next),
        WalkStep::LeafSlot(..) => Err(WalkError::NotATable),
    }
}

/// The physical address of the PTE table for `va`, walking the full three
/// intermediate levels from the root.
///
/// # Safety
///
/// `root_pa` must be an allocated PGD page, and the full path to `va` must
/// already exist (all table pointers live).
unsafe fn pte_table_pa<B: PhysBackend>(
    backend: &B,
    root_pa: u64,
    va: u64,
) -> Result<u64, WalkError> {
    let mut table_pa = root_pa;
    let mut level = Level::Pgd;
    for _ in 0..3 {
        // SAFETY: every visited table is an allocated page.
        table_pa = unsafe { descend(backend, table_pa, va, level) }?;
        level = level.next().unwrap_or(Level::Pte);
    }
    Ok(table_pa)
}

/// A four-level page table hierarchy rooted at one PGD page, walking through
/// a [`PhysBackend`].
///
/// This is the map/unmap engine behind [`crate::addrspace::AddressSpace`],
/// which owns frame allocation; the struct itself only allocates and frees
/// *table* pages through the backend.
pub struct PageTable<'a, B: PhysBackend> {
    backend: &'a mut B,
    root_pa: u64,
}

impl<'a, B: PhysBackend> PageTable<'a, B> {
    /// Adopt an existing root (e.g. one read from `CSR.PGDL`/`CSR.PGDH`).
    pub fn from_root(backend: &'a mut B, root_pa: u64) -> Self {
        Self { backend, root_pa }
    }

    /// The physical address of the root table — what `CSR.PGDL` (for
    /// user-half addresses) is loaded with when this address space is
    /// activated.
    pub fn root_pa(&self) -> u64 {
        self.root_pa
    }

    /// Map one 4 KiB page.
    ///
    /// Allocates table pages as needed; refuses to overwrite a live mapping
    /// (call [`unmap_page`] first).
    pub fn map_page(&mut self, va: u64, pa: u64, flags: PageFlags) -> Result<(), MapError> {
        if va & 0xFFF != 0 {
            return Err(MapError::MisalignedVirtual);
        }
        if pa & 0xFFF != 0 {
            return Err(MapError::MisalignedPhysical);
        }
        let pte_table_pa = match unsafe { self.ensure_tables(va) } {
            Ok(pa) => pa,
            Err(WalkError::OutOfTables) => return Err(MapError::OutOfMemory),
            // `ensure_tables` never hits a leaf on an empty path; any other
            // error is a corrupted table, reported as a refused mapping.
            Err(_) => return Err(MapError::AlreadyMapped),
        };
        let slot_pa = pte_table_pa + (pte_index(va) * 8) as u64;
        // SAFETY: slot is inside an allocated table page.
        let existing = unsafe { self.backend.read_phys(slot_pa) };
        if existing & sw::PRESENT != 0 {
            return Err(MapError::AlreadyMapped);
        }
        // SAFETY: same allocated slot, validated empty above.
        unsafe { self.backend.write_phys(slot_pa, make_leaf(pa, flags)) };
        Ok(())
    }

    /// Map `pages` consecutive 4 KiB pages, linearly.
    pub fn map_range(
        &mut self,
        va: u64,
        pa: u64,
        pages: u64,
        flags: PageFlags,
    ) -> Result<(), MapError> {
        for i in 0..pages {
            let step = i * PAGE_SIZE;
            self.map_page(va + step, pa + step, flags)?;
        }
        Ok(())
    }

    /// Update the flags of a mapped page.
    pub fn set_flags(&mut self, va: u64, flags: PageFlags) -> Result<(), MapError> {
        if va & 0xFFF != 0 {
            return Err(MapError::MisalignedVirtual);
        }
        let pte_table_pa = self.pte_table_for(va)?;
        let slot_pa = pte_table_pa + (pte_index(va) * 8) as u64;
        // SAFETY: slot is inside an allocated table page.
        let existing = unsafe { self.backend.read_phys(slot_pa) };
        if existing & sw::PRESENT == 0 {
            return Err(MapError::NotMapped);
        }
        // Rebuild the leaf, preserving the frame; `make_leaf` derives every
        // architectural bit from `flags`, so no stale `D`/`MAT`/`NX` survive.
        let entry = make_leaf(entry_pa(existing), flags) | (existing & hw::GLOBAL);
        // SAFETY: same allocated slot.
        unsafe { self.backend.write_phys(slot_pa, entry) };
        Ok(())
    }

    /// Remove a mapping, returning the physical address it held and freeing
    /// any table pages that become empty.
    pub fn unmap_page(&mut self, va: u64) -> Result<u64, MapError> {
        if va & 0xFFF != 0 {
            return Err(MapError::MisalignedVirtual);
        }
        let pte_table_pa = self.pte_table_for(va)?;
        let slot_pa = pte_table_pa + (pte_index(va) * 8) as u64;
        // SAFETY: slot is inside an allocated table page.
        let existing = unsafe { self.backend.read_phys(slot_pa) };
        if existing & sw::PRESENT == 0 || existing & hw::VALID == 0 {
            return Err(MapError::NotMapped);
        }
        let pa = entry_pa(existing);
        // SAFETY: same allocated slot.
        unsafe { self.backend.write_phys(slot_pa, 0) };
        self.reap_empty_tables(va);
        Ok(pa)
    }

    /// Remove `pages` consecutive 4 KiB mappings.
    pub fn unmap_range(&mut self, va: u64, pages: u64) -> Result<(), MapError> {
        for i in 0..pages {
            let step = i * PAGE_SIZE;
            self.unmap_page(va + step)?;
        }
        Ok(())
    }

    /// Translate `va` to its physical address, or `None` if unmapped.
    pub fn translate(&self, va: u64) -> Option<u64> {
        if va & 0xFFF != 0 {
            return None;
        }
        // SAFETY: the root and every descended table are allocated pages.
        let pte_table_pa = unsafe { pte_table_pa(self.backend, self.root_pa, va) }.ok()?;
        let slot_pa = pte_table_pa + (pte_index(va) * 8) as u64;
        // SAFETY: slot is inside an allocated table page.
        let entry = unsafe { self.backend.read_phys(slot_pa) };
        if entry & sw::PRESENT == 0 || entry & hw::VALID == 0 {
            return None;
        }
        Some(entry_pa(entry) | (va & 0xFFF))
    }

    /// Is `va` mapped to a live leaf page?
    pub fn is_mapped(&self, va: u64) -> bool {
        self.translate(va).is_some()
    }

    /// The physical address of the PTE table holding `va`'s slot.
    fn pte_table_for(&mut self, va: u64) -> Result<u64, MapError> {
        // SAFETY: root and all descended tables are allocated pages.
        unsafe { pte_table_pa(self.backend, self.root_pa, va) }.map_err(|_| MapError::NotMapped)
    }

    /// Walk down from the root, allocating missing tables, and return the
    /// PTE-table page for `va`.
    ///
    /// # Safety
    ///
    /// The root must be an allocated table page.
    unsafe fn ensure_tables(&mut self, va: u64) -> Result<u64, WalkError> {
        let mut table_pa = self.root_pa;
        let mut level = Level::Pgd;
        for _ in 0..3 {
            let slot_pa = table_pa + (index_of(va, level) * 8) as u64;
            // SAFETY: slot is inside an allocated table page.
            let entry = unsafe { self.backend.read_phys(slot_pa) };
            let next_pa = if is_table_pointer(entry) {
                entry_pa(entry)
            } else if is_leaf(entry) {
                return Err(WalkError::NotATable);
            } else {
                // Allocate the missing level.
                let fresh = self.backend.alloc_table().ok_or(WalkError::OutOfTables)?;
                // SAFETY: same slot, validated empty above.
                unsafe { self.backend.write_phys(slot_pa, make_table_pointer(fresh)) };
                fresh
            };
            table_pa = next_pa;
            level = level.next().unwrap_or(Level::Pte);
        }
        Ok(table_pa)
    }

    /// Free table pages along `va`'s path that became empty after an unmap.
    ///
    /// Walks from the root collecting the path (all levels still exist right
    /// after an unmap), then frees bottom-up while each table is fully zero.
    /// The root is never freed.
    fn reap_empty_tables(&mut self, va: u64) {
        let mut path = [0u64; 4];
        let mut depth = 0;
        let mut table_pa = self.root_pa;
        let mut level = Level::Pgd;
        loop {
            path[depth] = table_pa;
            depth += 1;
            match level.next() {
                Some(next) => {
                    // SAFETY: all visited tables are allocated pages.
                    match unsafe { descend(self.backend, table_pa, va, level) } {
                        Ok(next_pa) => table_pa = next_pa,
                        // The path was complete a moment ago; a failure now
                        // means corruption — leave everything alone.
                        Err(_) => return,
                    }
                    level = next;
                }
                None => break,
            }
        }
        // path[0..depth) = PGD, PUD, PMD, PTE tables along the address.
        for level_idx in (1..depth).rev() {
            let table_pa = path[level_idx];
            if !self.table_is_empty(table_pa) {
                return;
            }
            // SAFETY: `table_pa` was allocated by `ensure_tables` and is now
            // empty and unreferenced (its parent slot is cleared below).
            unsafe { self.backend.free_table(table_pa) };
            let parent = path[level_idx - 1];
            let slot_pa = parent + (index_of(va, parent_level(level_idx)) * 8) as u64;
            // SAFETY: parent is an allocated table page.
            unsafe { self.backend.write_phys(slot_pa, 0) };
        }
    }

    /// Does every entry in the 4 KiB table at `pa` read zero?
    fn table_is_empty(&self, pa: u64) -> bool {
        for slot in 0..ENTRIES_PER_TABLE {
            // SAFETY: every slot is inside the allocated table page.
            if unsafe { self.backend.read_phys(pa + (slot * 8) as u64) } != 0 {
                return false;
            }
        }
        true
    }
}

/// The level *above* the table at `level_idx` in the path built by
/// [`PageTable::reap_empty_tables`] (PGD = 0 … PTE = 3).
fn parent_level(level_idx: usize) -> Level {
    match level_idx {
        1 => Level::Pgd,
        2 => Level::Pud,
        _ => Level::Pmd,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;
    use core::cell::{Cell, RefCell};
    use std::vec::Vec;

    /// Fake 64 MiB of physical memory for hosted walks.
    ///
    /// The bookkeeping lives behind interior mutability: a test reads the
    /// counters while the walker still holds the backend mutably.
    struct FakePhys {
        mem: Vec<u8>,
        next_table: Cell<u64>,
        freed: RefCell<Vec<u64>>,
    }

    impl FakePhys {
        const SIZE: usize = 1 << 26; // 64 MiB

        fn new() -> Self {
            Self {
                mem: vec![0; Self::SIZE],
                // Page 0 stays a "never allocated" sentinel.
                next_table: Cell::new(0x1000),
                freed: RefCell::new(Vec::new()),
            }
        }
    }

    impl PhysBackend for FakePhys {
        unsafe fn read_phys(&self, pa: u64) -> u64 {
            let off = pa as usize;
            assert!(off + 8 <= self.mem.len(), "out-of-bounds phys read {pa:#x}");
            let bytes: [u8; 8] = self.mem[off..off + 8].try_into().unwrap();
            u64::from_le_bytes(bytes)
        }

        unsafe fn write_phys(&mut self, pa: u64, value: u64) {
            let off = pa as usize;
            assert!(
                off + 8 <= self.mem.len(),
                "out-of-bounds phys write {pa:#x}"
            );
            self.mem[off..off + 8].copy_from_slice(&value.to_le_bytes());
        }

        fn alloc_table(&mut self) -> Option<u64> {
            let pa = self.next_table.get();
            if pa as usize + PAGE_SIZE as usize > Self::SIZE {
                return None;
            }
            self.next_table.set(self.next_table.get() + PAGE_SIZE);
            Some(pa)
        }

        unsafe fn free_table(&mut self, pa: u64) {
            let off = pa as usize;
            self.mem[off..off + PAGE_SIZE as usize].fill(0);
            self.freed.borrow_mut().push(pa);
        }
    }

    #[test]
    fn indices_split_the_va_space_like_linux() {
        // Linux pgtable.h: PGDIR_SHIFT 39, PUD 30, PMD 21, PTE 12.
        let va: u64 = 0x0000_1234_5678_9ABC;
        assert_eq!(pgd_index(va), ((va >> 39) & 0x1FF) as usize);
        assert_eq!(pud_index(va), ((va >> 30) & 0x1FF) as usize);
        assert_eq!(pmd_index(va), ((va >> 21) & 0x1FF) as usize);
        assert_eq!(pte_index(va), ((va >> 12) & 0x1FF) as usize);
    }

    #[test]
    fn leaf_layout_matches_entrylo() {
        let pa = 0x0000_0001_2345_6000u64;
        let entry = make_leaf(pa, PageFlags::kernel_rw());
        assert_eq!(entry & hw::VALID, hw::VALID);
        assert_eq!(entry & hw::DIRTY, hw::DIRTY, "kernel_rw is writable");
        // A page that is not executable says so: the NX bit is the
        // architecture's "no execute" and VelaOS's kernel pages are data.
        assert_eq!(
            entry & hw::NO_EXEC,
            hw::NO_EXEC,
            "NX set: kernel_rw is data"
        );
        assert_eq!(entry & hw::PPN_MASK, pa);
        assert_eq!(entry_pa(entry), pa);
        assert!(is_leaf(entry));
        assert!(!is_table_pointer(entry));

        // User read-only executable page: PLV 3, D clear, NX clear.
        let entry = make_leaf(pa, PageFlags::user_rx());
        assert_eq!((entry >> 2) & 0b11, 0b11, "PLV 3 for user pages");
        assert_eq!(entry & hw::DIRTY, 0, "read-only pages have D clear");
        assert_eq!(entry & hw::NO_EXEC, 0, "executable pages have NX clear");

        // Kernel RWX is a policy violation upstream of this module; here the
        // D bit must reflect writability regardless.
        let entry = make_leaf(pa, PageFlags::kernel_ro());
        assert_eq!(entry & hw::DIRTY, 0);
        assert_eq!(entry & hw::NO_EXEC, hw::NO_EXEC);

        // MMIO: SUC memory access type.
        let entry = make_leaf(pa, PageFlags::mmio());
        assert_eq!(MemAccess::from_bits(entry >> 4), MemAccess::StrongUncached);
        let entry = make_leaf(pa, PageFlags::kernel_rw());
        assert_eq!(MemAccess::from_bits(entry >> 4), MemAccess::CoherentCached);
    }

    #[test]
    fn global_leaf_matches_every_asid() {
        let entry = make_leaf_global(0x1000, PageFlags::kernel_rw());
        assert_eq!(entry & hw::GLOBAL, hw::GLOBAL);
        assert!(is_leaf(entry));
    }

    #[test]
    fn table_pointers_are_not_leaves() {
        let entry = make_table_pointer(0x2000);
        assert!(is_table_pointer(entry));
        assert!(!is_leaf(entry));
        assert_eq!(entry_pa(entry), 0x2000);
        // `V` clear: a naive TLB fill from this word would fault, not mis-map.
        assert_eq!(entry & hw::VALID, 0);
    }

    #[test]
    fn tlbidx_encodes_index_ps_and_ne() {
        // INDEX bits 11:0, PS bits 29:24 (12 for 4 KiB), NE bit 31.
        let value = tlbidx_value(0x123);
        assert_eq!(value & 0xFFF, 0x123);
        assert_eq!((value >> 24) & 0x3F, 12);
        assert_eq!(value >> 31, 0);
        let value = tlbidx_invalidate(0x123);
        assert_eq!(value >> 31, 1, "NE set for invalidation");
    }

    #[test]
    fn tlbehi_carries_vppn() {
        // QEMU writes `address & (TARGET_PAGE_MASK << 1)` into `CSR_TLBEHI`,
        // so the register keeps the virtual page number in bits 47:13 —
        // the page offset *and* the bit below it are cleared.
        let va = 0x0000_1234_5678_9000u64;
        assert_eq!(tlbehi_value(va), va & 0xFFFF_FFFF_E000);
        assert_eq!(vppn_of(va), va >> 13);
    }

    #[test]
    fn dmw_values_match_linux_inits() {
        // Linux's 64-bit initialisation values (arch/loongarch/include/asm/
        // loongarch.h): the VSEG field is bits 63:60, `MAT` is bits 5:4 and
        // the PLV mask is bits 3:0, so
        //   CSR_DMW0_INIT = 0x8000_0000_0000_0001  (VSEG 0x8, PLV0, MAT 0)
        //   CSR_DMW1_INIT = 0x9000_0000_0000_0011  (VSEG 0x9, PLV0, MAT 1)
        //   CSR_DMW2_INIT = 0xA000_0000_0000_0021  (VSEG 0xA, PLV0, MAT 2)
        let dmw0 = dmw_kernel_suc();
        assert_eq!(dmw0, 0x8000_0000_0000_0001);
        assert_eq!(dmw0 >> 60, 0x8);
        assert_eq!(dmw0 & 0xF, 0b0001);
        assert_eq!((dmw0 >> 4) & 0b11, 0b00, "MAT 0: strongly-ordered uncached");
        let dmw1 = dmw_kernel_cc();
        assert_eq!(dmw1, 0x9000_0000_0000_0011);
        assert_eq!(dmw1 >> 60, 0x9);
        assert_eq!((dmw1 >> 4) & 0b11, 0b01, "MAT 1: coherent cached");
        let dmw2 = dmw_value(DMW_WUC_VSEG, 0b0001, MemAccess::WeakUncached);
        assert_eq!(dmw2, 0xA000_0000_0000_0021);
        assert_eq!((dmw2 >> 4) & 0b11, 0b10, "MAT 2: weak-ordered uncached");

        // Window bases and translation agree with the MMU's arithmetic.
        assert_eq!(dmw_virtual_base(DMW_KERNEL_CC_VSEG), 0x9000_0000_0000_0000);
        assert_eq!(dmw_translate(0x9000_0000_1FE0_01E0, 0x9), 0x1FE0_01E0);
    }

    #[test]
    fn pwcl_pwch_describe_the_four_levels() {
        // PTBASE 12/PTWIDTH 9, DIR1 21/9, DIR2 30/9, PTEWIDTH 3;
        // DIR3 39/9; hardware walker disabled.
        let pwcl = pwcl_value();
        assert_eq!(pwcl & 0x1F, 12);
        assert_eq!((pwcl >> 5) & 0x1F, 9);
        assert_eq!((pwcl >> 10) & 0x1F, 21);
        assert_eq!((pwcl >> 15) & 0x1F, 9);
        assert_eq!((pwcl >> 20) & 0x1F, 30);
        assert_eq!((pwcl >> 25) & 0x1F, 9);
        assert_eq!((pwcl >> 30) & 0b11, 3);
        let pwch = pwch_value();
        assert_eq!(pwch & 0x3F, 39);
        assert_eq!((pwch >> 6) & 0x3F, 9);
        assert_eq!(pwch & (1 << 24), 0, "hardware walker stays disabled");
    }

    #[test]
    fn map_translate_roundtrip() {
        let mut phys = FakePhys::new();
        let root = phys.alloc_table().expect("root");
        let mut table = PageTable::from_root(&mut phys, root);

        let va = 0x0000_1000_2000_3000u64;
        let pa = 0x0000_0000_DEAD_B000u64;
        table.map_page(va, pa, PageFlags::kernel_rw()).unwrap();
        assert!(table.is_mapped(va));
        assert_eq!(table.translate(va), Some(pa | (va & 0xFFF)));
        assert!(!table.is_mapped(va + 0x1000));
    }

    #[test]
    fn double_map_is_refused() {
        let mut phys = FakePhys::new();
        let root = phys.alloc_table().unwrap();
        let mut table = PageTable::from_root(&mut phys, root);
        let va = 0x0000_4000_0000_0000u64;
        table.map_page(va, 0x1000, PageFlags::kernel_rw()).unwrap();
        assert_eq!(
            table.map_page(va, 0x2000, PageFlags::kernel_rw()),
            Err(MapError::AlreadyMapped)
        );
    }

    #[test]
    fn misaligned_requests_are_refused() {
        let mut phys = FakePhys::new();
        let root = phys.alloc_table().unwrap();
        let mut table = PageTable::from_root(&mut phys, root);
        assert_eq!(
            table.map_page(0x1001, 0x2000, PageFlags::kernel_rw()),
            Err(MapError::MisalignedVirtual)
        );
        assert_eq!(
            table.map_page(0x1000, 0x2001, PageFlags::kernel_rw()),
            Err(MapError::MisalignedPhysical)
        );
    }

    #[test]
    fn unmap_frees_empty_tables() {
        let mut phys = FakePhys::new();
        let root = phys.alloc_table().unwrap();

        // Two pages that share no intermediate tables with anything else.
        let va = 0x0000_0000_0007_F000u64;
        {
            let mut table = PageTable::from_root(&mut phys, root);
            table
                .map_page(va, 0x10_000, PageFlags::kernel_rw())
                .unwrap();
            assert_eq!(table.unmap_page(va).unwrap(), 0x10_000);
            assert!(!table.is_mapped(va));
            // And the root still translates nothing.
            assert_eq!(table.translate(va), None);
        }
        // The walker has let go of the backend, so the bookkeeping can be
        // read: PGD/PUD/PMD/PTE along the path all became empty and were
        // freed, and only the root survives.
        let freed = phys.freed.borrow();
        assert_eq!(freed.len(), 3, "PTE+PMD+PUD freed");
        assert!(freed.iter().all(|pa| *pa != root));
    }

    #[test]
    fn unmap_keeps_shared_tables() {
        let mut phys = FakePhys::new();
        let root = phys.alloc_table().unwrap();

        // Two pages in the same PMD range: the PMD must survive unmapping
        // only one of them.
        let va0 = 0x0000_0000_0007_F000u64;
        let va1 = va0 + 0x1000;
        {
            let mut table = PageTable::from_root(&mut phys, root);
            table
                .map_page(va0, 0x10_000, PageFlags::kernel_rw())
                .unwrap();
            table
                .map_page(va1, 0x11_000, PageFlags::kernel_rw())
                .unwrap();
            assert_eq!(table.unmap_page(va0).unwrap(), 0x10_000);
            assert!(table.is_mapped(va1));
        }
        // Nothing was freed: the table still holds the second page.
        assert!(phys.freed.borrow().is_empty(), "PTE table still holds va1");
    }

    #[test]
    fn unmap_unmapped_is_not_mapped() {
        let mut phys = FakePhys::new();
        let root = phys.alloc_table().unwrap();
        let mut table = PageTable::from_root(&mut phys, root);
        assert_eq!(table.unmap_page(0x1000), Err(MapError::NotMapped));
    }

    #[test]
    fn set_flags_flips_writable_and_exec() {
        let mut phys = FakePhys::new();
        let root = phys.alloc_table().unwrap();
        let va = 0x0000_0000_0100_0000u64;
        {
            let mut table = PageTable::from_root(&mut phys, root);
            table
                .map_page(va, 0x20_000, PageFlags::kernel_ro())
                .unwrap();
            table.set_flags(va, PageFlags::kernel_rw()).unwrap();
        }
        // The walker has let go of the backend, so the leaf can be read.
        let entry = {
            let pte_pa =
                unsafe { pte_table_pa(&phys, root, va) }.unwrap() + (pte_index(va) * 8) as u64;
            unsafe { phys.read_phys(pte_pa) }
        };
        assert_eq!(entry & hw::DIRTY, hw::DIRTY, "writable after set_flags");

        {
            let mut table = PageTable::from_root(&mut phys, root);
            table.set_flags(va, PageFlags::kernel_ro()).unwrap();
        }
        let entry = {
            let pte_pa =
                unsafe { pte_table_pa(&phys, root, va) }.unwrap() + (pte_index(va) * 8) as u64;
            unsafe { phys.read_phys(pte_pa) }
        };
        assert_eq!(entry & hw::DIRTY, 0, "clean after ro set_flags");
    }

    #[test]
    fn map_range_and_unmap_range() {
        let mut phys = FakePhys::new();
        let root = phys.alloc_table().unwrap();
        let mut table = PageTable::from_root(&mut phys, root);

        let va = 0x0000_0002_0000_0000u64;
        let pa = 0x0000_0003_0000_0000u64;
        table
            .map_range(va, pa, 100, PageFlags::kernel_rw())
            .unwrap();
        for i in 0..100u64 {
            assert_eq!(
                table.translate(va + i * PAGE_SIZE),
                Some(pa + i * PAGE_SIZE)
            );
        }
        table.unmap_range(va, 100).unwrap();
        for i in 0..100u64 {
            assert_eq!(table.translate(va + i * PAGE_SIZE), None);
        }
    }

    #[test]
    fn walk_step_distinguishes_leaf_and_table() {
        let table_entry = make_table_pointer(0x3000);
        assert_eq!(
            walk_step(0x1000, Level::Pgd, table_entry),
            Ok(WalkStep::Table(0x3000))
        );
        let leaf_entry = make_leaf(0x4000, PageFlags::kernel_rw());
        assert_eq!(
            walk_step(0x1000, Level::Pte, leaf_entry),
            Ok(WalkStep::LeafSlot(0x4000, pte_index(0x1000)))
        );
        // A leaf at an intermediate level is a huge page: refused.
        assert_eq!(
            walk_step(0x1000, Level::Pud, leaf_entry),
            Err(WalkError::NotATable)
        );
        // An empty slot cannot be descended into.
        assert_eq!(
            walk_step(0x1000, Level::Pgd, 0),
            Err(WalkError::BadTablePointer)
        );
    }
}
