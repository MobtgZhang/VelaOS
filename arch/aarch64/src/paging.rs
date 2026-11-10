//! AArch64 stage-1 translation: EL1, 4 KiB granule, four levels, 48-bit VA.
//!
//! # Structure
//!
//! With a 4 KiB granule and `T0SZ`/`T1SZ` = 16, stage-1 translation divides a
//! 48-bit virtual address into four 9-bit table indices and a 12-bit page
//! offset:
//!
//! ```text
//! 63      48 47    39 38    30 29    21 20    12 11     0
//! +----------+--------+--------+--------+--------+---------+
//! | ASID/tag |   L0   |   L1   |   L2   |   L3   | offset  |
//! +----------+--------+--------+--------+--------+---------+
//! ```
//!
//! The high bits 63..=48 are not part of the address: the low half (all zero)
//! is translated through `TTBR0_EL1` and belongs to userspace, the high half
//! (all ones) through `TTBR1_EL1` and is the kernel window, starting at
//! [`KERNEL_VIRTUAL_BASE`]. Anything else is an unmapped canonical address
//! and the hardware raises a translation fault without consulting the tables
//! at all.
//!
//! Each level is an array of 512 8-byte descriptors indexed by its table
//! index. A level 0–2 descriptor is either a *table* descriptor (pointer to
//! the next level) or a *block* descriptor (a 512 GiB / 1 GiB / 2 MiB leaf);
//! a level 3 descriptor is always a 4 KiB *page* descriptor. This crate
//! creates only 4 KiB pages and table pointers, but [`translate`] honours a
//! block it encounters, which is what the hardware does.
//!
//! # Descriptor layout (ARMv8-A ARM, DDI 0487, "VMSA" — stage-1 descriptor
//! format for block and page descriptors, 4 KiB granule)
//!
//! ```text
//! 63        55 54  53                    12 11  10 9 8 7 6 5 4 2 1 0
//! +-----------+----+-------------------------+----+---+----+---+-----+---+
//! | reserved  |UXN |PXN| CONT DBM |  OA[47:12] | NG |AF | SH |AP2|AI  | V |
//! +-----------+----+-------------------------+----+---+----+---+-----+---+
//! ```
//!
//! (Not to scale: the `OA` field is bits 47:12, `AI` the attribute index
//! bits 4:2, `AP` bits 7:6, `SH` bits 9:8.)
//!
//! * `V` (Valid, bit 0) must be set on every entry in use, leaf or pointer.
//! * Bit 1: for level 3, *must* be 1 (page descriptor). For levels 0–2, 1
//!   means *table* and 0 means *block*. Table descriptors therefore carry
//!   exactly this bit plus the next-table address — the same encoding a page
//!   descriptor has, disambiguated only by the level.
//! * `AI` (AttrIndx, bits 4:2) selects one of the eight attributes in
//!   `MAIR_EL1`. VelaOS uses index 0 = Normal write-back cacheable and
//!   index 1 = Device-nGnRE (see [`MAIR_ATTR_DEVICE_NGNRE`]).
//! * `AP` (bits 7:6): `AP[2]` = read-only, `AP[1]` = EL0-accessible. The four
//!   encodings are `0b00` EL1 R/W, `0b01` EL0+EL1 R/W, `0b10` EL1 R only,
//!   `0b11` EL0+EL1 R only. Note there is no "write without read" encoding
//!   to avoid — unlike RISC-V, W naturally implies R.
//! * `SH` (bits 9:8): shareability. `0b11` = Inner Shareable for Normal
//!   memory; Device memory is programmed non-shareable (`0b00`).
//! * `AF` (bit 10, Access Flag): an access to a valid entry with `AF` clear
//!   raises an Access Flag fault (ESR `DFSC` `0b0010xx`) instead of
//!   succeeding. VelaOS sets it at map time — the software-managed analogue
//!   of RISC-V's A/D bits — rather than taking a first-touch fault per page.
//! * `NG` (bit 11, Not Global): set for user pages, so the entry is tagged
//!   with the ASID and does not match in every address space; kernel pages
//!   are global and shared through `TTBR1`.
//! * `OA` (bits 47:12): the output physical address, minus its low 12 bits —
//!   which is exactly why every table and every mapped frame must be
//!   page-aligned: the alignment *is* the field's zero padding.
//! * `PXN` (bit 53, Privileged eXecute-Never) and `UXN` (bit 54, User eXN)
//!   carry the W^X policy: VelaOS sets both on every non-executable page,
//!   sets `PXN` on user-executable pages (so the kernel cannot be tricked
//!   into fetching from a user mapping) and clears both only on kernel code.
//! * Bits 63:59 of *table* descriptors are the upper attributes (`PXNTable`,
//!   `XNTable`, `APTable`, `NSTable`) that constrain the whole subtree; VelaOS
//!   leaves them clear and manages permissions per leaf.
//!
//! # Testing
//!
//! The walk itself is pure: [`translate`], [`map_page`], [`unmap_page`] and
//! [`set_flags`] operate through the [`PhysBackend`] trait, which reads and
//! writes 64-bit descriptors at physical addresses. The unit tests drive them
//! with a mock backend, so the four-level walk, the error cases and the flag
//! encodings are all verified on the host without any ARM hardware.

use vela_arch_common::{MapError, PageFlags, PAGE_SIZE};

/// Entries per table level: 512, selected by 9 index bits.
pub const ENTRIES_PER_TABLE: usize = 512;

/// Bytes occupied by one page-table level (exactly one 4 KiB frame).
pub const TABLE_BYTES: usize = ENTRIES_PER_TABLE * core::mem::size_of::<u64>();

/// The number of translation levels the kernel uses (level 0 root through
/// level 3 pages).
pub const LEVELS: usize = 4;

/// The width of the translated virtual address: 48 bits, four 9-bit indices
/// plus the 12-bit page offset.
pub const VA_BITS: u64 = 48;

/// `TCR_EL1.T0SZ`/`T1SZ` for a 48-bit VA space: 64 − 48 = 16.
pub const TSZ_48BIT: u64 = 64 - VA_BITS;

/// The highest physical address a 4 KiB-granule `OA` field can express: bits
/// 47:12, so a physical address at or above 2^48 cannot be encoded.
pub const MAX_PHYSICAL: u64 = 1 << 48;

/// Where the kernel window begins: the high half of the 48-bit space (bits
/// 63:48 all ones) is translated through `TTBR1_EL1`, the low half through
/// `TTBR0_EL1`.
pub const KERNEL_VIRTUAL_BASE: u64 = 0xFFFF_8000_0000_0000;

/// The mask selecting the `OA` field of a leaf or table descriptor.
pub const OA_MASK: u64 = 0x0000_FFFF_FFFF_F000;

/// True when `va` is a canonical 48-bit address: bits 63..=48 must all be
/// zero (the `TTBR0` half) or all ones (the `TTBR1` half). A non-canonical
/// address never reaches the tables — the hardware raises a translation
/// fault first — so the kernel must reject one before writing a mapping.
pub const fn is_canonical(va: u64) -> bool {
    let high = va >> 48;
    high == 0 || high == 0xFFFF
}

/// True when `va` falls in the kernel's `TTBR1` half.
pub const fn is_kernel_canonical(va: u64) -> bool {
    va >= KERNEL_VIRTUAL_BASE
}

// ---------------------------------------------------------------------------
// Descriptor encodings.
// ---------------------------------------------------------------------------

/// Descriptor bit flags, from the stage-1 descriptor layout above.
pub mod pte_flags {
    /// Valid: the entry is in use (leaf or pointer).
    pub const VALID: u64 = 1 << 0;
    /// Table descriptor bit (bit 1). On levels 0–2, 1 = *table* and 0 =
    /// *block*; on level 3 the bit is mandatory 1 and means *page*.
    pub const TABLE: u64 = 1 << 1;
    /// The `AttrIndx` field (bits 4:2), selecting a `MAIR_EL1` attribute.
    pub const ATTRINDX_MASK: u64 = 0b111 << 2;
    /// `AP[1]` (bit 6): EL0-accessible when set.
    pub const AP_USER: u64 = 1 << 6;
    /// `AP[2]` (bit 7): read-only when set.
    pub const AP_RDONLY: u64 = 1 << 7;
    /// The `SH` shareability field (bits 9:8).
    pub const SH_MASK: u64 = 0b11 << 8;
    /// `AF` (bit 10, Access Flag): software-set at map time; an access with
    /// the flag clear faults with `DFSC` `0b0010xx`.
    pub const AF: u64 = 1 << 10;
    /// `NG` (bit 11, Not Global): tag the entry with the ASID.
    pub const NG: u64 = 1 << 11;
    /// `PXN` (bit 53): privileged execute-never.
    pub const PXN: u64 = 1 << 53;
    /// `UXN` (bit 54): user execute-never.
    pub const UXN: u64 = 1 << 54;
}

/// `MAIR_EL1` attribute index 0: Normal memory, write-back, read/write
/// allocate (MAIR attribute `0xFF`).
pub const ATTRIDX_NORMAL: u64 = 0;

/// `MAIR_EL1` attribute index 1: Device-nGnRE — no gathering, no reordering,
/// early write acknowledgement permitted (MAIR attribute `0x04`). Used for
/// [`PageFlags::mmio`] mappings.
pub const ATTRIDX_DEVICE: u64 = 1;

/// The `MAIR_EL1` attribute byte for Normal write-back cacheable memory.
pub const MAIR_ATTR_NORMAL_WB: u64 = 0xFF;

/// The `MAIR_EL1` attribute byte for Device-nGnRE memory.
pub const MAIR_ATTR_DEVICE_NGNRE: u64 = 0x04;

/// `SH` encoding: Inner Shareable — the domain QEMU's `virt` machine and a
/// typical SMP SoC run in.
pub const SH_INNER_SHAREABLE: u64 = 0b11;

/// The `AttrIndx` value a descriptor carries for `flags`.
pub const fn attrindx_of(flags: PageFlags) -> u64 {
    if flags.uncacheable {
        ATTRIDX_DEVICE
    } else {
        ATTRIDX_NORMAL
    }
}

/// The `SH` field a descriptor carries for `flags`: Normal memory is Inner
/// Shareable; Device memory is non-shareable (`0b00`), per the ARMv8-A ARM's
/// Device-memory shareability rules.
pub const fn sh_of(flags: PageFlags) -> u64 {
    if flags.uncacheable {
        0b00
    } else {
        SH_INNER_SHAREABLE
    }
}

/// The `AP` field (bits 7:6) a descriptor carries for `flags`.
///
/// The ARMv8-A ARM's four AP encodings map directly onto [`PageFlags`]:
/// `AP[1]` is the user bit and `AP[2]` the read-only bit. There is no
/// reserved "write without read" encoding on this architecture.
pub const fn ap_of(flags: PageFlags) -> u64 {
    let user = if flags.user { pte_flags::AP_USER } else { 0 };
    let ro = if flags.writable {
        0
    } else {
        pte_flags::AP_RDONLY
    };
    user | ro
}

/// Build a *page* descriptor (level 3 leaf) mapping `pa` with `flags`.
///
/// Sets `V`, the mandatory level-3 table/page bit, `AF`, the `AttrIndx`/`SH`
/// fields for the memory type, the `AP` field for the permissions, and the
/// `PXN`/`UXN` pair for the W^X policy:
///
/// * non-executable pages get both `PXN` and `UXN` — no bit in the descriptor
///   then allows execution;
/// * user-executable pages get `PXN` only (the kernel must not fetch from a
///   user mapping);
/// * kernel-executable pages get neither.
///
/// `NG` is set for user pages so they are tagged with the ASID and do not
/// match across address spaces; kernel pages stay global because they live in
/// the shared `TTBR1` tree.
pub const fn pte_new(phys: u64, flags: PageFlags) -> u64 {
    let mut out = pte_flags::VALID
        | pte_flags::TABLE
        | pte_flags::AF
        | (attrindx_of(flags) << 2)
        | (sh_of(flags) << 8)
        | ap_of(flags)
        | (phys & OA_MASK);
    if flags.executable && flags.user {
        // User code: privileged execute-never keeps the kernel from fetching
        // through this mapping.
        out |= pte_flags::PXN;
    } else if !flags.executable {
        // W^X: a non-executable page must allow execution from nowhere.
        out |= pte_flags::PXN | pte_flags::UXN;
    }
    if flags.user {
        out |= pte_flags::NG;
    }
    out
}

/// Build a *table* descriptor pointing at the next-level table.
///
/// Only `V` and the table bit are set: the descriptor is the same shape as a
/// page descriptor, disambiguated from it only by the level that reads it.
/// The upper attributes (bits 63:59 — `PXNTable`/`XNTable`/`APTable`/
/// `NSTable`) are left clear; permissions are managed per leaf. The table
/// address must be page-aligned because its low 12 bits are consumed by the
/// `OA` field.
pub const fn pte_new_table(table_pa: u64) -> u64 {
    pte_flags::VALID | pte_flags::TABLE | (table_pa & OA_MASK)
}

/// True when the descriptor is valid — the only bit the hardware demands
/// before it will use an entry at all.
pub const fn pte_valid(pte: u64) -> bool {
    pte & pte_flags::VALID != 0
}

/// True when a valid level 0–2 descriptor is a *table* pointer. Its negation
/// (valid, table bit clear) is a *block* leaf at that level.
pub const fn pte_is_table(pte: u64) -> bool {
    pte & pte_flags::TABLE != 0
}

/// The span, in bytes, of a block leaf found at `level` (0-based): 512 GiB at
/// level 0, 1 GiB at level 1, 2 MiB at level 2 — each level above the 4 KiB
/// page multiplies the span by 512.
pub const fn block_span(level: usize) -> u64 {
    1u64 << (12 + 9 * (LEVELS as u64 - 1 - level as u64))
}

/// The physical page number encoded in a leaf descriptor, shifted into a
/// physical address. For a non-leaf descriptor this is the address of the
/// next table.
pub const fn pte_pa(pte: u64) -> u64 {
    pte & OA_MASK
}

/// The raw `OA` field (bits 47:12) of a descriptor.
pub const fn pte_ppn(pte: u64) -> u64 {
    (pte & OA_MASK) >> 12
}

/// The page flags of a valid leaf descriptor, for diagnostics and tests.
///
/// `readable` reports the `AP` field's meaning: on this architecture every
/// writable encoding is readable, so the promotion is expressed by the
/// hardware field rather than by the encoder.
pub const fn pte_flags_of(pte: u64) -> PageFlags {
    let user = pte & pte_flags::AP_USER != 0;
    let ro = pte & pte_flags::AP_RDONLY != 0;
    let nx_mask = pte_flags::PXN | pte_flags::UXN;
    let executable = pte & nx_mask != nx_mask;
    PageFlags {
        readable: true,
        writable: !ro,
        executable,
        user,
        user_writable: user && !ro,
        uncacheable: (pte & pte_flags::ATTRINDX_MASK) >> 2 == ATTRIDX_DEVICE,
    }
}

/// A decomposed 48-bit stage-1 virtual address.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Split {
    /// Root table index (virtual address bits 47:39).
    pub l0: usize,
    /// Level 1 table index (bits 38:30).
    pub l1: usize,
    /// Level 2 table index (bits 29:21).
    pub l2: usize,
    /// Level 3 table index (bits 20:12).
    pub l3: usize,
    /// Byte offset within the 4 KiB page.
    pub offset: u64,
}

impl Split {
    /// Decompose `va`.
    pub const fn of(va: u64) -> Self {
        Self {
            l0: ((va >> 39) & 0x1FF) as usize,
            l1: ((va >> 30) & 0x1FF) as usize,
            l2: ((va >> 21) & 0x1FF) as usize,
            l3: ((va >> 12) & 0x1FF) as usize,
            offset: va & 0xFFF,
        }
    }

    /// Reassemble the page-aligned base of this split. The high bits are
    /// re-derived from the root index — bit 47 is the top bit of `l0`, and a
    /// set bit 47 means the `TTBR1` half whose bits 63:48 are all ones —
    /// exactly as the hardware interprets them, so `base_of() + offset`
    /// round-trips every canonical address.
    pub const fn base_of(&self) -> u64 {
        let low = ((self.l0 as u64) << 39)
            | ((self.l1 as u64) << 30)
            | ((self.l2 as u64) << 21)
            | ((self.l3 as u64) << 12);
        if self.l0 >= 256 {
            low | 0xFFFF_0000_0000_0000
        } else {
            low
        }
    }

    /// The four indices in root-to-leaf order.
    pub const fn path(&self) -> [usize; LEVELS] {
        [self.l0, self.l1, self.l2, self.l3]
    }
}

/// Read/write access to page-table memory by *physical* address.
///
/// The walk code in this module never dereferences pointers itself: it asks a
/// backend to read or write one 64-bit descriptor at a physical address, and
/// to hand out a zeroed, page-aligned frame for a new table. On the target
/// the concrete backend is supplied by the kernel (whose heap is identity-
/// mapped during boot, so a table's physical address is the pointer to it —
/// see [`crate::addrspace`]); on the host the tests supply a mock, which is
/// what makes the whole walk unit-testable.
pub trait PhysBackend {
    /// Read the 64-bit descriptor stored at physical address `pa`.
    ///
    /// # Errors
    ///
    /// [`MapError::NotMapped`] when the backend cannot service the address —
    /// for the heap pool this means the caller named a table that was never
    /// allocated, which is a kernel bug and not a user-visible condition.
    fn read_pte(&mut self, pa: u64) -> Result<u64, MapError>;

    /// Write a 64-bit descriptor at physical address `pa`.
    ///
    /// # Errors
    ///
    /// [`MapError::NotMapped`] when no table backs `pa`, as in
    /// [`Self::read_pte`].
    fn write_pte(&mut self, pa: u64, value: u64) -> Result<(), MapError>;

    /// Allocate a zeroed, page-aligned frame for a new table and return its
    /// physical address.
    ///
    /// # Errors
    ///
    /// [`MapError::OutOfMemory`] when the platform has no frame left.
    fn alloc_table(&mut self) -> Result<u64, MapError>;
}

/// The descriptor address of entry `index` of the table at `table_pa`.
///
/// Exposed because both the walk and the address-space deep-clone need the
/// same arithmetic, and because it is the one place the "descriptors are 8
/// bytes, tables are 4 KiB" invariant lives.
pub const fn pte_address(table_pa: u64, index: usize) -> u64 {
    table_pa + (index as u64) * 8
}

/// Translate `va` in the space rooted at `root_pa`.
///
/// The walk mirrors the hardware's four levels exactly: read the level 0
/// entry selected by `l0`, descend if it is a table pointer, then `l1`, then
/// `l2`, then `l3`, and return the leaf's physical address plus the page
/// offset. A block leaf found at level 0/1/2 is honoured by adding the offset
/// masked to that level's span, which is what the hardware does.
///
/// # Errors
///
/// [`MapError::NotMapped`] only when the *backend* fails (a table that was
/// never allocated). An absent mapping is `Ok(None)`, because "not mapped" is
/// an ordinary state of an address space, not an error.
pub fn translate(
    backend: &mut dyn PhysBackend,
    root_pa: u64,
    va: u64,
) -> Result<Option<u64>, MapError> {
    let split = Split::of(va);
    let mut table_pa = root_pa;
    // Levels 0, 1 and 2: descend through table entries, honouring blocks.
    for (level, index) in [(0usize, split.l0), (1, split.l1), (2, split.l2)] {
        let pte = backend.read_pte(pte_address(table_pa, index))?;
        if !pte_valid(pte) {
            return Ok(None);
        }
        if !pte_is_table(pte) {
            // A block leaf: the block's base is the descriptor's output
            // address aligned down to this level's span, and the low bits
            // of `va` name the byte within it. A well-formed descriptor
            // already carries an aligned address; the masking is what the
            // hardware does with one that does not.
            let span = block_span(level);
            let base = pte_pa(pte) & !(span - 1);
            return Ok(Some(base + (va & (span - 1))));
        }
        table_pa = pte_pa(pte);
    }
    let pte = backend.read_pte(pte_address(table_pa, split.l3))?;
    if !pte_valid(pte) {
        return Ok(None);
    }
    Ok(Some(pte_pa(pte) + split.offset))
}

/// Map one 4 KiB page of `pa` at `va` in the space rooted at `root_pa`.
///
/// Intermediate tables on the path are allocated through the backend as
/// needed. Re-mapping the *same* frame is not an error: it is how a caller
/// updates flags, and the entry is simply re-encoded. A different frame in an
/// occupied slot is [`MapError::AlreadyMapped`].
///
/// # Errors
///
/// * [`MapError::MisalignedVirtual`] — `va` not page-aligned or not canonical.
/// * [`MapError::MisalignedPhysical`] — `pa` not page-aligned.
/// * [`MapError::AddressOverflow`] — `pa` is outside the 2^48 range the `OA`
///   field can encode.
/// * [`MapError::AlreadyMapped`] — the slot holds a different mapping, or a
///   block descriptor already covers the range.
/// * [`MapError::OutOfMemory`] — the backend had no frame for a new table.
pub fn map_page(
    backend: &mut dyn PhysBackend,
    root_pa: u64,
    va: u64,
    pa: u64,
    flags: PageFlags,
) -> Result<(), MapError> {
    if !va.is_multiple_of(PAGE_SIZE) {
        return Err(MapError::MisalignedVirtual);
    }
    if !pa.is_multiple_of(PAGE_SIZE) {
        return Err(MapError::MisalignedPhysical);
    }
    if !is_canonical(va) {
        return Err(MapError::MisalignedVirtual);
    }
    if pa >= MAX_PHYSICAL {
        return Err(MapError::AddressOverflow);
    }
    let split = Split::of(va);
    let mut table_pa = root_pa;
    // Descend, creating missing intermediate tables as pointer entries.
    for index in [split.l0, split.l1, split.l2] {
        let slot = pte_address(table_pa, index);
        let pte = backend.read_pte(slot)?;
        if !pte_valid(pte) {
            let child = backend.alloc_table()?;
            backend.write_pte(slot, pte_new_table(child))?;
            table_pa = child;
        } else if pte_is_table(pte) {
            table_pa = pte_pa(pte);
        } else {
            // A block already covers this range; the caller asked for a 4 KiB
            // mapping under it, which would conflict.
            return Err(MapError::AlreadyMapped);
        }
    }
    let slot = pte_address(table_pa, split.l3);
    let existing = backend.read_pte(slot)?;
    if pte_valid(existing) && pte_pa(existing) != (pa & OA_MASK) {
        return Err(MapError::AlreadyMapped);
    }
    backend.write_pte(slot, pte_new(pa, flags))
}

/// Remove the mapping at `va`, returning the physical frame it held.
///
/// The intermediate tables above the leaf are *not* returned to the backend:
/// like the x86-64 implementation, unmapping a page leaves its (possibly
/// shared) tables in place, and only the leaf frame goes back to the caller.
///
/// # Errors
///
/// [`MapError::NotMapped`] when `va` has no valid 4 KiB leaf, or
/// [`MapError::MisalignedVirtual`] when `va` is not page-aligned.
pub fn unmap_page(backend: &mut dyn PhysBackend, root_pa: u64, va: u64) -> Result<u64, MapError> {
    if !va.is_multiple_of(PAGE_SIZE) {
        return Err(MapError::MisalignedVirtual);
    }
    let leaf = match leaf_slot(backend, root_pa, va)? {
        Some(slot) => slot,
        None => return Err(MapError::NotMapped),
    };
    let (slot, existing) = leaf;
    backend.write_pte(slot, 0)?;
    Ok(pte_pa(existing))
}

/// Change the flags of an existing mapping without moving its frame.
///
/// # Errors
///
/// [`MapError::NotMapped`] when `va` has no valid 4 KiB leaf, or
/// [`MapError::MisalignedVirtual`] when `va` is not page-aligned.
pub fn set_flags(
    backend: &mut dyn PhysBackend,
    root_pa: u64,
    va: u64,
    flags: PageFlags,
) -> Result<(), MapError> {
    if !va.is_multiple_of(PAGE_SIZE) {
        return Err(MapError::MisalignedVirtual);
    }
    let leaf = match leaf_slot(backend, root_pa, va)? {
        Some(slot) => slot,
        None => return Err(MapError::NotMapped),
    };
    let (slot, existing) = leaf;
    backend.write_pte(slot, pte_new(pte_pa(existing), flags))
}

/// The level 3 slot backing `va`, as `(slot address, current descriptor)`.
///
/// `Ok(None)` means no valid 4 KiB leaf backs `va` — the path is absent, or a
/// block descriptor covers the range instead of a table below it.
fn leaf_slot(
    backend: &mut dyn PhysBackend,
    root_pa: u64,
    va: u64,
) -> Result<Option<(u64, u64)>, MapError> {
    let split = Split::of(va);
    let mut table_pa = root_pa;
    for index in [split.l0, split.l1, split.l2] {
        let pte = backend.read_pte(pte_address(table_pa, index))?;
        if !pte_valid(pte) || !pte_is_table(pte) {
            // No path, or a block where this walker expects a pointer:
            // either way there is no 4 KiB leaf below.
            return Ok(None);
        }
        table_pa = pte_pa(pte);
    }
    let slot = pte_address(table_pa, split.l3);
    let existing = backend.read_pte(slot)?;
    if !pte_valid(existing) {
        return Ok(None);
    }
    Ok(Some((slot, existing)))
}

// ---------------------------------------------------------------------------
// System-register value builders. All pure, so the bit layouts are pinned by
// the host tests below rather than discovered on hardware.
// ---------------------------------------------------------------------------

/// `TCR_EL1.T0SZ`, bits 5:0 — the size log-2 complement of the `TTBR0`
/// (user) space.
pub const TCR_T0SZ_SHIFT: u64 = 0;
/// `TCR_EL1.IRGN0`, bits 9:8 — inner cacheability of `TTBR0` table walks.
pub const TCR_IRGN0_SHIFT: u64 = 8;
/// `TCR_EL1.ORGN0`, bits 11:10 — outer cacheability of `TTBR0` table walks.
pub const TCR_ORGN0_SHIFT: u64 = 10;
/// `TCR_EL1.SH0`, bits 13:12 — shareability of `TTBR0` table walks.
pub const TCR_SH0_SHIFT: u64 = 12;
/// `TCR_EL1.TG0`, bits 15:14 — `TTBR0` granule size (`0b00` = 4 KiB).
pub const TCR_TG0_SHIFT: u64 = 14;
/// `TCR_EL1.T1SZ`, bits 21:16 — the size log-2 complement of the `TTBR1`
/// (kernel) space.
pub const TCR_T1SZ_SHIFT: u64 = 16;
/// `TCR_EL1.EPD1`, bit 23 — disabled-`TTBR1`-walks bit; VelaOS leaves it
/// clear because kernel mappings live in `TTBR1`.
pub const TCR_EPD1_SHIFT: u64 = 23;
/// `TCR_EL1.IRGN1`, bits 25:24 — inner cacheability of `TTBR1` table walks.
pub const TCR_IRGN1_SHIFT: u64 = 24;
/// `TCR_EL1.ORGN1`, bits 27:26 — outer cacheability of `TTBR1` table walks.
pub const TCR_ORGN1_SHIFT: u64 = 26;
/// `TCR_EL1.SH1`, bits 29:28 — shareability of `TTBR1` table walks.
pub const TCR_SH1_SHIFT: u64 = 28;
/// `TCR_EL1.TG1`, bits 31:30 — `TTBR1` granule size (`0b10` = 4 KiB; the
/// `TTBR1` encoding differs from `TG0`'s).
pub const TCR_TG1_SHIFT: u64 = 30;
/// `TCR_EL1.IPS`, bits 34:32 — the intermediate physical address size.
pub const TCR_IPS_SHIFT: u64 = 32;
/// `TCR_EL1.A1`, bit 35 — take the ASID from `TTBR1_EL1` (1) or `TTBR0_EL1`
/// (0). VelaOS keeps it clear: the ASID rides in `TTBR0_EL1` bits 63:48 with
/// each user space, and the kernel tree is ASID-untagged.
pub const TCR_A1_SHIFT: u64 = 35;
/// `TCR_EL1.AS`, bit 36 — 16-bit ASIDs (1) or 8-bit (0). VelaOS sets it.
pub const TCR_AS_SHIFT: u64 = 36;
/// `TCR_EL1.HA`, bit 39 — hardware access-flag update (ARMv8.1). VelaOS sets
/// the `AF` bit in software instead and leaves this clear.
pub const TCR_HA_SHIFT: u64 = 39;

/// `TCR_EL1.TG0`/`TG1` encoding for a 4 KiB granule (`TG0` field).
pub const TG0_4KB: u64 = 0b00;
/// `TCR_EL1.TG1` encoding for a 4 KiB granule (`TG1` field; note the
/// different bit pattern from [`TG0_4KB`]).
pub const TG1_4KB: u64 = 0b10;
/// `IRGN`/`ORGN` encoding for Normal memory, write-back, read/write
/// allocate — what table walks use.
pub const IRGN_ORGN_WB_RW_ALLOC: u64 = 0b11;
/// `IPS` encoding: 48-bit intermediate physical addresses (256 TiB).
pub const IPS_48BIT: u64 = 0b101;

/// Build a `TCR_EL1` value for a 48-bit, 4 KiB-granule translation regime.
///
/// The register layout (ARMv8-A ARM, "TCR_EL1"):
///
/// ```text
/// 63                                   48 47       32 31 30 29 28 27 26
/// +--------------------------------------+-----------+----+----+----+----+
/// |                    reserved          |  IPS/HA.. |TG1 |SH1 |ORGN1|IRGN1
/// ...
/// 25 24 23        21 20 16 15 14 13 12 11 10 9  8  7   5    0
/// +----+----+--------+-------+----+----+----+----+----+----+-----+
/// |IRGN1  |EPD1|  T1SZ |TG1 ...  SH0 |ORGN0|IRGN0|EPD0| T0SZ    |
/// +----+----+--------+-------+----+----+----+----+----+----+-----+
/// ```
///
/// (Not to scale; see the [`TCR_*_SHIFT`] constants for the exact positions.)
///
/// * `T0SZ` = `T1SZ` = 16: both halves are 48 bits wide.
/// * `TG0` = `0b00` and `TG1` = `0b10`: 4 KiB granules (the two fields use
///   *different* encodings for the same granule — a classic trap).
/// * `SH0`/`SH1` = Inner Shareable, `IRGN`/`ORGN` = write-back allocate: the
///   table walkers may cache tables in the coherent domain.
/// * `IPS` selects the physical address size; `0b101` = 48-bit, matching
///   [`MAX_PHYSICAL`]. A platform with fewer physical address bits must pass
///   its own encoding.
/// * `A1` clear (ASID from `TTBR0_EL1`) and `AS` set (16-bit ASIDs).
pub const fn tcr_el1_value(ips: u64) -> u64 {
    (TSZ_48BIT << TCR_T0SZ_SHIFT)
        | (IRGN_ORGN_WB_RW_ALLOC << TCR_IRGN0_SHIFT)
        | (IRGN_ORGN_WB_RW_ALLOC << TCR_ORGN0_SHIFT)
        | (SH_INNER_SHAREABLE << TCR_SH0_SHIFT)
        | (TG0_4KB << TCR_TG0_SHIFT)
        | (TSZ_48BIT << TCR_T1SZ_SHIFT)
        | (IRGN_ORGN_WB_RW_ALLOC << TCR_IRGN1_SHIFT)
        | (IRGN_ORGN_WB_RW_ALLOC << TCR_ORGN1_SHIFT)
        | (SH_INNER_SHAREABLE << TCR_SH1_SHIFT)
        | (TG1_4KB << TCR_TG1_SHIFT)
        | ((ips & 0b111) << TCR_IPS_SHIFT)
        | (1 << TCR_AS_SHIFT)
}

/// `SCTLR_EL1.M`, bit 0 — the MMU enable. Clear here: the builder's job is to
/// *set* it as the last step of the boot sequence.
pub const SCTLR_M: u64 = 1 << 0;
/// `SCTLR_EL1.C`, bit 2 — data cache enable.
pub const SCTLR_C: u64 = 1 << 2;
/// `SCTLR_EL1.SA`, bit 3 — SP alignment checking at EL1. The AAPCS64 requires
/// 16-byte-aligned stack pointers at public interfaces; the hardware check
/// makes a misalignment a fault instead of a corruption.
pub const SCTLR_SA: u64 = 1 << 3;
/// `SCTLR_EL1.SA0`, bit 4 — SP alignment checking at EL0.
pub const SCTLR_SA0: u64 = 1 << 4;
/// `SCTLR_EL1.I`, bit 12 — instruction cache enable.
pub const SCTLR_I: u64 = 1 << 12;
/// `SCTLR_EL1.UCI`, bit 26 — EL0 access to cache-maintenance instructions.
/// VelaOS leaves it clear: user code has no use for `DC`/`IC` under W^X.
pub const SCTLR_UCI: u64 = 1 << 26;

/// Build an `SCTLR_EL1` value that turns the MMU on over `current`.
///
/// Only the bits a kernel boot needs are touched: `M` (MMU), `C` (d-cache),
/// `I` (i-cache) and the two stack-alignment checks. `EE` (bit 25) stays
/// clear — little-endian — and `UCI` stays clear. The caller must have
/// written `TCR_EL1`, `MAIR_EL1` and both TTBRs, and issued the required
/// barriers, *before* this value goes live; enabling the MMU under an
/// unconfigured translation regime traps the core.
pub const fn sctlr_el1_for_mmu_enable(current: u64) -> u64 {
    current | SCTLR_M | SCTLR_C | SCTLR_I | SCTLR_SA | SCTLR_SA0
}

/// Build a `MAIR_EL1` value with VelaOS's two attributes.
///
/// The register is eight independent bytes; byte *n* is the attribute
/// selected by descriptor `AttrIndx` *n*:
///
/// * index 0 = Normal write-back cacheable (`0xFF`) — all of RAM;
/// * index 1 = Device-nGnRE (`0x04`) — MMIO;
/// * the remaining six are left at `0x00` (Device-nGnRnE) and unused.
pub const fn mair_el1_value() -> u64 {
    MAIR_ATTR_NORMAL_WB | (MAIR_ATTR_DEVICE_NGNRE << 8)
}

/// The `TTBR0_EL1`/`TTBR1_EL1` ASID field, bits 63:48 (16-bit ASIDs, per
/// `TCR.AS`).
pub const TTBR_ASID_SHIFT: u64 = 48;

/// Build a `TTBR0_EL1`/`TTBR1_EL1` value for a root table and ASID.
///
/// The register layout (ARMv8-A ARM, "TTBR0_EL1"): `ASID` bits 63:48 and
/// `BADDR` bits 47:1 — the root table's physical address with its bit 0
/// ignored. A root table is 4 KiB-aligned (its low 12 bits must be zero for
/// the entry pointing at it to be expressible), so `BADDR`'s ignored low
/// bits are naturally clear.
pub const fn ttbr_value(root_pa: u64, asid: u16) -> u64 {
    ((asid as u64) << TTBR_ASID_SHIFT) | (root_pa & OA_MASK)
}

/// The root-table physical address encoded in a TTBR value.
pub const fn ttbr_pa(ttbr: u64) -> u64 {
    ttbr & OA_MASK
}

/// The ASID encoded in a TTBR value.
pub const fn ttbr_asid(ttbr: u64) -> u16 {
    (ttbr >> TTBR_ASID_SHIFT) as u16
}

#[cfg(test)]
extern crate std;

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::vec::Vec;

    /// A mock [`PhysBackend`]: a map of descriptor slots plus a bump
    /// allocator for table frames. Every walk in the tests runs against this.
    struct MockMem {
        slots: BTreeMap<u64, u64>,
        next_frame: u64,
        frames: Vec<u64>,
    }

    impl MockMem {
        fn new() -> Self {
            Self {
                slots: BTreeMap::new(),
                next_frame: 0x1000,
                frames: Vec::new(),
            }
        }

        fn allocated_tables(&self) -> usize {
            self.frames.len()
        }
    }

    impl PhysBackend for MockMem {
        fn read_pte(&mut self, pa: u64) -> Result<u64, MapError> {
            self.slots.get(&pa).copied().ok_or(MapError::NotMapped)
        }

        fn write_pte(&mut self, pa: u64, value: u64) -> Result<(), MapError> {
            if !self.slots.contains_key(&pa) {
                return Err(MapError::NotMapped);
            }
            self.slots.insert(pa, value);
            Ok(())
        }

        fn alloc_table(&mut self) -> Result<u64, MapError> {
            let frame = self.next_frame;
            self.next_frame += PAGE_SIZE;
            // A zeroed table: every slot present and empty.
            for i in 0..ENTRIES_PER_TABLE {
                self.slots.insert(pte_address(frame, i), 0);
            }
            self.frames.push(frame);
            Ok(frame)
        }
    }

    fn root_backend() -> (MockMem, u64) {
        let mut mem = MockMem::new();
        let root = mem.alloc_table().expect("root frame");
        (mem, root)
    }

    #[test]
    fn tcr_layout_matches_the_spec() {
        // Every field must land in its own bit range: a shift typo silently
        // reconfigures the translation regime.
        let tcr = tcr_el1_value(IPS_48BIT);
        assert_eq!(tcr & 0x3F, TSZ_48BIT, "T0SZ in bits 5:0");
        assert_eq!((tcr >> TCR_T1SZ_SHIFT) & 0x3F, TSZ_48BIT, "T1SZ");
        assert_eq!((tcr >> TCR_TG0_SHIFT) & 0b11, TG0_4KB);
        assert_eq!((tcr >> TCR_TG1_SHIFT) & 0b11, TG1_4KB);
        assert_eq!((tcr >> TCR_SH0_SHIFT) & 0b11, SH_INNER_SHAREABLE);
        assert_eq!((tcr >> TCR_SH1_SHIFT) & 0b11, SH_INNER_SHAREABLE);
        for shift in [
            TCR_IRGN0_SHIFT,
            TCR_ORGN0_SHIFT,
            TCR_IRGN1_SHIFT,
            TCR_ORGN1_SHIFT,
        ] {
            assert_eq!((tcr >> shift) & 0b11, IRGN_ORGN_WB_RW_ALLOC);
        }
        assert_eq!((tcr >> TCR_IPS_SHIFT) & 0b111, IPS_48BIT);
        assert_eq!(tcr & (1 << TCR_A1_SHIFT), 0, "ASID comes from TTBR0");
        assert_ne!(tcr & (1 << TCR_AS_SHIFT), 0, "16-bit ASIDs");
        assert_eq!(tcr & (1 << TCR_HA_SHIFT), 0, "AF is software-set");
        // The two granule fields encode the same granule differently.
        assert_ne!(TG0_4KB, TG1_4KB);
    }

    #[test]
    fn mair_carries_the_two_attributes() {
        let mair = mair_el1_value();
        assert_eq!(mair & 0xFF, MAIR_ATTR_NORMAL_WB, "index 0 = normal WB");
        assert_eq!(
            (mair >> 8) & 0xFF,
            MAIR_ATTR_DEVICE_NGNRE,
            "index 1 = device"
        );
        // The remaining six attribute slots are unused and zero.
        for index in 2..8u64 {
            assert_eq!((mair >> (index * 8)) & 0xFF, 0);
        }
    }

    #[test]
    fn sctlr_builder_sets_mmu_and_caches_over_current() {
        let current = 0x0030_5000; // some platform-set bits
        let value = sctlr_el1_for_mmu_enable(current);
        assert_eq!(value & SCTLR_M, SCTLR_M);
        assert_eq!(value & SCTLR_C, SCTLR_C);
        assert_eq!(value & SCTLR_I, SCTLR_I);
        assert_eq!(value & SCTLR_SA, SCTLR_SA);
        assert_eq!(value & SCTLR_SA0, SCTLR_SA0);
        assert_eq!(value & SCTLR_UCI, 0);
        // The builder ORs: platform bits survive untouched.
        assert_eq!(value & !0b1111_1111, current & !0b1111_1111);
    }

    #[test]
    fn ttbr_layout_matches_the_spec() {
        let ttbr = ttbr_value(0x0012_3000, 0xBEEF);
        assert_eq!(ttbr_asid(ttbr), 0xBEEF, "ASID in bits 63:48");
        assert_eq!(ttbr_pa(ttbr), 0x0012_3000, "BADDR in bits 47:1");
        // A 4 KiB-aligned root contributes no low bits, so the ASID and the
        // address never collide.
        assert_eq!(ttbr_value(0x1000, 0xFFFF) & 0xFFF, 0);
    }

    #[test]
    fn pte_round_trips_per_flags_variant() {
        // Each variant maps to the documented descriptor bit pattern.
        // The columns are the AP field, then PXN, UXN and NG.
        //
        // The policy the encoder documents: a page that may not be executed
        // sets *both* execute-never bits; a user page that may be executed
        // sets only the privileged one, so the kernel cannot fetch through
        // it; and a user page is not global, so it does not match across
        // address spaces.
        let cases: [(PageFlags, u64, bool, bool, bool); 6] = [
            (PageFlags::kernel_rw(), 0b00, true, true, false),
            (PageFlags::kernel_rx(), 0b10, false, false, false),
            (PageFlags::kernel_ro(), 0b10, true, true, false),
            (PageFlags::user_rw(), 0b01, true, true, true),
            (PageFlags::user_rx(), 0b11, true, false, true),
            (PageFlags::mmio(), 0b00, true, true, false),
        ];
        for (flags, ap, pxn, uxn, ng) in cases {
            let pte = pte_new(0x0DEA_0000, flags);
            assert!(pte_valid(pte), "every map is valid");
            assert!(pte_is_table(pte), "a level-3 page descriptor has bit 1 set");
            assert_eq!(
                pte & (pte_flags::AP_USER | pte_flags::AP_RDONLY),
                ap << 6,
                "AP field encodes {flags:?}"
            );
            assert_eq!(pte & pte_flags::PXN != 0, pxn);
            assert_eq!(pte & pte_flags::UXN != 0, uxn);
            assert_eq!(pte & pte_flags::NG != 0, ng);
            assert_eq!(pte_pa(pte), 0x0DEA_0000);
        }
    }

    #[test]
    fn ap_field_encodes_the_four_permission_families() {
        // AP[2:1]: 0b00 EL1 R/W, 0b01 EL0+EL1 R/W, 0b10 EL1 R, 0b11 EL0+EL1 R.
        assert_eq!(ap_of(PageFlags::kernel_rw()), 0b00 << 6);
        assert_eq!(ap_of(PageFlags::user_rw()), 0b01 << 6);
        assert_eq!(ap_of(PageFlags::kernel_ro()), 0b10 << 6);
        assert_eq!(ap_of(PageFlags::user_rx()), 0b11 << 6);
        // Writable implies readable on this architecture: the encoding has
        // no reserved W-without-R pattern to avoid.
        let write_only = PageFlags {
            readable: false,
            writable: true,
            ..PageFlags::kernel_rw()
        };
        assert_eq!(ap_of(write_only), ap_of(PageFlags::kernel_rw()));
    }

    #[test]
    fn pxn_and_uxn_carry_the_wx_policy() {
        // Non-executable: both execute-never bits set.
        for flags in [
            PageFlags::kernel_rw(),
            PageFlags::user_rw(),
            PageFlags::kernel_ro(),
        ] {
            let pte = pte_new(0x1000, flags);
            assert_ne!(pte & pte_flags::PXN, 0);
            assert_ne!(pte & pte_flags::UXN, 0);
        }
        // Kernel code: executable at EL1 and at EL0 (which cannot reach it
        // through AP anyway).
        let kernel_code = pte_new(0x2000, PageFlags::kernel_rx());
        assert_eq!(kernel_code & (pte_flags::PXN | pte_flags::UXN), 0);
        // User code: executable at EL0, never at EL1.
        let user_code = pte_new(0x3000, PageFlags::user_rx());
        assert_ne!(user_code & pte_flags::PXN, 0);
        assert_eq!(user_code & pte_flags::UXN, 0);
    }

    #[test]
    fn mmio_selects_the_device_attribute_and_non_shareable_sh() {
        let pte = pte_new(0x1000_1000, PageFlags::mmio());
        assert_eq!(pte & pte_flags::ATTRINDX_MASK, ATTRIDX_DEVICE << 2);
        assert_eq!(pte & pte_flags::SH_MASK, 0b00 << 8);
        assert!(pte_flags_of(pte).uncacheable);
        // Ordinary RAM pages are Normal, inner-shareable.
        let pte = pte_new(0x1000, PageFlags::kernel_rw());
        assert_eq!(pte & pte_flags::ATTRINDX_MASK, ATTRIDX_NORMAL << 2);
        assert_eq!(pte & pte_flags::SH_MASK, SH_INNER_SHAREABLE << 8);
        assert!(!pte_flags_of(pte).uncacheable);
    }

    #[test]
    fn user_pages_are_not_global() {
        assert_ne!(pte_new(0x2000, PageFlags::user_rx()) & pte_flags::NG, 0);
        assert_eq!(pte_new(0x2000, PageFlags::kernel_rw()) & pte_flags::NG, 0);
    }

    #[test]
    fn every_map_carries_the_access_flag() {
        // Software sets AF at map time; a descriptor without it would take a
        // first-touch Access Flag fault instead.
        for flags in [
            PageFlags::kernel_rw(),
            PageFlags::kernel_rx(),
            PageFlags::kernel_ro(),
            PageFlags::user_rw(),
            PageFlags::user_rx(),
            PageFlags::mmio(),
        ] {
            assert_ne!(pte_new(0x4000, flags) & pte_flags::AF, 0);
        }
    }

    #[test]
    fn flags_decode_round_trips() {
        for flags in [
            PageFlags::kernel_rw(),
            PageFlags::kernel_rx(),
            PageFlags::kernel_ro(),
            PageFlags::user_rw(),
            PageFlags::user_rx(),
            PageFlags::mmio(),
        ] {
            let pte = pte_new(0x3000, flags);
            let back = pte_flags_of(pte);
            assert!(back.readable, "every AP encoding is readable");
            assert_eq!(back.writable, flags.writable);
            assert_eq!(back.executable, flags.executable);
            assert_eq!(back.user, flags.user);
            assert_eq!(back.user_writable, flags.user_writable);
            assert_eq!(back.uncacheable, flags.uncacheable);
        }
    }

    #[test]
    fn table_pointers_carry_only_valid_and_table_bits() {
        let pte = pte_new_table(0x5000);
        assert!(pte_valid(pte));
        assert!(pte_is_table(pte), "a pointer has the table bit");
        assert_eq!(pte_pa(pte), 0x5000);
        // Flags and address never collide: the low 12 bits of the table
        // address are consumed by the OA field.
        assert_eq!(pte_new_table(0x5001) & OA_MASK, 0x5000);
    }

    #[test]
    fn split_round_trips() {
        for va in [
            0u64,
            0x1000,
            0x0000_0040_0000_1000,
            KERNEL_VIRTUAL_BASE,
            KERNEL_VIRTUAL_BASE + 0x0123_4567,
            0xFFFF_FFFF_FFFF_F000,
        ] {
            assert!(is_canonical(va), "test input must be canonical");
            let s = Split::of(va);
            assert_eq!(s.base_of() + s.offset, va, "round-trip failed for {va:#x}");
        }
    }

    #[test]
    fn non_canonical_addresses_are_recognised() {
        // With a 48-bit virtual address, bits 63:48 must be all-clear (the
        // user half, `TTBR0_EL1`) or all-ones (the kernel half,
        // `TTBR1_EL1`); anything else translation-faults before the
        // hardware ever reads a table.
        assert!(!is_canonical(0x0001_0000_0000_0000));
        assert!(!is_canonical(0xFFFE_0000_0000_0000));
        // The whole 48-bit user half is canonical, including its top.
        assert!(is_canonical(0x0000_8000_0000_0000));
        assert!(is_canonical(0x0000_7FFF_FFFF_F000));
        // And so is the whole kernel half.
        assert!(is_canonical(0xFFFF_8000_0000_0000));
        assert!(is_canonical(0xFFFF_FFFF_FFFF_F000));
    }

    #[test]
    fn split_indices_are_in_range() {
        for i in 0..4096u64 {
            let s = Split::of(i.wrapping_mul(0x1234_5678_9ABC).wrapping_mul(0x1F));
            for idx in s.path() {
                assert!(idx < ENTRIES_PER_TABLE);
            }
            assert!(s.offset < PAGE_SIZE);
        }
    }

    #[test]
    fn map_translate_unmap_walk() {
        let (mut mem, root) = root_backend();
        let va = 0x0000_1234_5000;
        let pa = 0x0000_0ABC_D000;

        map_page(&mut mem, root, va, pa, PageFlags::user_rx()).expect("map");
        assert_eq!(
            translate(&mut mem, root, va).expect("walk"),
            Some(pa),
            "translate resolves the mapped frame"
        );
        assert_eq!(translate(&mut mem, root, va + 0x1000).unwrap(), None);

        let freed = unmap_page(&mut mem, root, va).expect("unmap");
        assert_eq!(freed, pa, "unmap returns the frame it removed");
        assert_eq!(translate(&mut mem, root, va).unwrap(), None);
        assert_eq!(unmap_page(&mut mem, root, va), Err(MapError::NotMapped));
    }

    #[test]
    fn a_kernel_address_walks_the_same_tree_shape() {
        // The kernel half of a 48-bit space is the top sixteen terabytes, so
        // its level-0 indices run from 256 to 511.
        let (mut mem, root) = root_backend();
        let va = KERNEL_VIRTUAL_BASE + 0x1000;
        map_page(&mut mem, root, va, 0x7000, PageFlags::kernel_rx()).expect("map");
        assert_eq!(Split::of(va).l0, 256);
        assert_eq!(translate(&mut mem, root, va).unwrap(), Some(0x7000));
        assert!(is_kernel_canonical(va));
    }

    #[test]
    fn remapping_the_same_frame_updates_flags() {
        let (mut mem, root) = root_backend();
        let va = 0x2000;
        map_page(&mut mem, root, va, 0x7000, PageFlags::user_rw()).expect("map");
        // Same frame, different flags: a flags update, not a conflict.
        map_page(&mut mem, root, va, 0x7000, PageFlags::kernel_ro()).expect("remap");
        // The walk is computed first: nesting the reads inside the outer
        // call would borrow the backend twice at once.
        let mut table = root;
        for idx in [Split::of(va).l0, Split::of(va).l1, Split::of(va).l2] {
            table = pte_pa(mem.read_pte(pte_address(table, idx)).unwrap());
        }
        let pte = mem.read_pte(pte_address(table, Split::of(va).l3)).unwrap();
        assert!(!pte_flags_of(pte).user, "flags were re-encoded");
        assert!(!pte_flags_of(pte).writable);
    }

    #[test]
    fn remapping_a_different_frame_is_rejected() {
        let (mut mem, root) = root_backend();
        map_page(&mut mem, root, 0x3000, 0x8000, PageFlags::kernel_rw()).expect("map");
        assert_eq!(
            map_page(&mut mem, root, 0x3000, 0x9000, PageFlags::kernel_rw()),
            Err(MapError::AlreadyMapped)
        );
    }

    #[test]
    fn set_flags_keeps_the_frame() {
        let (mut mem, root) = root_backend();
        map_page(&mut mem, root, 0x4000, 0xA000, PageFlags::kernel_rw()).expect("map");
        set_flags(&mut mem, root, 0x4000, PageFlags::kernel_ro()).expect("set_flags");
        assert_eq!(translate(&mut mem, root, 0x4000).unwrap(), Some(0xA000));
        assert_eq!(
            set_flags(&mut mem, root, 0x5000, PageFlags::kernel_ro()),
            Err(MapError::NotMapped)
        );
    }

    #[test]
    fn misaligned_and_overflow_errors_match_the_common_surface() {
        let (mut mem, root) = root_backend();
        assert_eq!(
            map_page(&mut mem, root, 0x1800, 0x1000, PageFlags::kernel_rw()),
            Err(MapError::MisalignedVirtual)
        );
        assert_eq!(
            map_page(&mut mem, root, 0x1000, 0x1800, PageFlags::kernel_rw()),
            Err(MapError::MisalignedPhysical)
        );
        // Above 2^48: the OA field cannot encode it.
        assert_eq!(
            map_page(&mut mem, root, 0x1000, MAX_PHYSICAL, PageFlags::kernel_rw()),
            Err(MapError::AddressOverflow)
        );
        // Non-canonical virtual address: bits 63:48 are neither all-clear
        // nor all-ones.
        assert_eq!(
            map_page(
                &mut mem,
                root,
                0x0001_0000_0000_0000,
                0x1000,
                PageFlags::kernel_rw()
            ),
            Err(MapError::MisalignedVirtual)
        );
    }

    #[test]
    fn unmapped_walks_report_not_mapped() {
        let (mut mem, root) = root_backend();
        assert_eq!(translate(&mut mem, root, 0x6000).unwrap(), None);
        assert_eq!(unmap_page(&mut mem, root, 0x6000), Err(MapError::NotMapped));
        assert_eq!(
            set_flags(&mut mem, root, 0x6000, PageFlags::kernel_rw()),
            Err(MapError::NotMapped)
        );
    }

    #[test]
    fn four_pages_share_one_leaf_table() {
        let (mut mem, root) = root_backend();
        // Four neighbouring 4 KiB pages share a level-3 table but need one
        // new level-2 table each above the first.
        for i in 0..4u64 {
            map_page(
                &mut mem,
                root,
                0x7000 + i * PAGE_SIZE,
                0xB000 + i * PAGE_SIZE,
                PageFlags::kernel_rw(),
            )
            .expect("map");
        }
        // The root, the level-1 table, the level-2 table and the leaf table
        // — four in all, because the pages are consecutive and share the
        // last two.
        assert_eq!(mem.allocated_tables(), 4);
        for i in 0..4u64 {
            assert_eq!(
                translate(&mut mem, root, 0x7000 + i * PAGE_SIZE).unwrap(),
                Some(0xB000 + i * PAGE_SIZE)
            );
        }
    }

    #[test]
    fn block_leaves_are_honoured_by_translate() {
        // A 2 MiB block at level 2: the table bit clear, and the output
        // address aligned to the block's size.
        let (mut mem, root) = root_backend();
        let two_mib = 2 * 1024 * 1024;
        // A page is mapped first, so that the tables on the path exist; the
        // block descriptor then replaces the level-2 entry.
        map_page(&mut mem, root, two_mib, 0x7000, PageFlags::kernel_rw()).expect("map");
        let block = pte_flags::VALID | (two_mib & OA_MASK);
        // The level-2 entry lives in the level-2 table, so the walk has to
        // reach that table to find it.
        let level0 = mem
            .read_pte(pte_address(root, Split::of(two_mib).l0))
            .unwrap();
        let level1 = mem
            .read_pte(pte_address(pte_pa(level0), Split::of(two_mib).l1))
            .unwrap();
        mem.slots
            .insert(pte_address(pte_pa(level1), Split::of(two_mib).l2), block);
        // The walk resolves the whole range from the single descriptor.
        assert_eq!(
            translate(&mut mem, root, two_mib + 0x1234).unwrap(),
            Some(two_mib + 0x1234)
        );
        // Blocks conflict with 4 KiB mappings underneath them.
        assert_eq!(
            map_page(
                &mut mem,
                root,
                two_mib + 0x1000,
                0x9000,
                PageFlags::kernel_rw()
            ),
            Err(MapError::AlreadyMapped)
        );
        assert_eq!(
            unmap_page(&mut mem, root, two_mib),
            Err(MapError::NotMapped),
            "unmap only removes 4 KiB leaves"
        );
    }

    #[test]
    fn block_spans_match_the_levels() {
        assert_eq!(block_span(0), 1 << 39, "512 GiB at level 0");
        assert_eq!(block_span(1), 1 << 30, "1 GiB at level 1");
        assert_eq!(block_span(2), 1 << 21, "2 MiB at level 2");
    }

    #[test]
    fn table_geometry_is_consistent() {
        assert_eq!(TABLE_BYTES, 4096);
        assert_eq!(ENTRIES_PER_TABLE * 8, 4096);
        assert_eq!(pte_address(0x1000, 511), 0x1000 + 511 * 8);
        assert_eq!(pte_address(0x1000, 512), 0x2000, "next table starts here");
        assert_eq!(LEVELS, 4);
        assert_eq!(TSZ_48BIT, 16);
    }

    #[test]
    fn pte_ppn_extracts_the_page_number() {
        let pte = pte_new(0x00AB_C000, PageFlags::kernel_rw());
        assert_eq!(pte_ppn(pte), 0x00AB_C000 >> 12);
        assert_eq!(pte_pa(pte), 0x00AB_C000);
    }
}
