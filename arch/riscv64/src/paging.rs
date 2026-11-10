//! Sv39 page tables for RV64 in supervisor mode.
//!
//! # Structure
//!
//! Sv39 divides a 39-bit virtual address into three 9-bit virtual page
//! numbers and a 12-bit page offset:
//!
//! ```text
//! 63        39 38    30 29    21 20    12 11     0
//! +----------+--------+--------+--------+---------+
//! | sign ext |  VPN2  |  VPN1  |  VPN0  | offset  |
//! +----------+--------+--------+--------+---------+
//! ```
//!
//! The high bits 63..=38 must all equal bit 38 (the address is *sign
//! extended*); anything else is an unmapped canonical address and the hardware
//! raises a page fault without consulting the tables at all. The low half
//! (bit 38 clear) covers 256 GiB of user space; the high half (bit 38 set) is
//! the kernel window, starting at [`KERNEL_VIRTUAL_BASE`].
//!
//! Each level is an array of 512 8-byte PTEs indexed by its VPN. Three levels:
//! the root table (indexed by VPN2), the middle table (VPN1) and the leaf
//! table (VPN0). A PTE whose R/W/X bits are all clear is a *pointer* to the
//! next table; a PTE with any of R, W, X set is a *leaf* mapping at that
//! level, which is also how 1 GiB megapages are expressed (they are not
//! produced by this crate, but [`translate`] handles one it encounters).
//!
//! # PTE layout (privileged spec, Vol. II "Sv39 Translation")
//!
//! ```text
//! 63  62-61 60      54 53          10 9 8 7 6 5 4 3 2 1 0
//! +---+------+------+--------------+---+-+-+-+-+--+-+-+-+
//! |N  | PBMT | RSVD |   PPN[53:10] |RSW|D|A|G|U|X|W|R|V|
//! +---+------+------+--------------+---+-+-+-+-+--+-+-+-+
//! ```
//!
//! * `V` (valid) must be set on every entry in use, leaf or pointer.
//! * `R`/`W`/`X` are read/write/execute. A PTE with W set and R clear is a
//!   *reserved encoding* — the spec reserves it for future use and it may
//!   fault — so a writable page is always made readable too (see [`pte_new`]).
//! * `U` marks user-accessible pages. The kernel can only touch a user page
//!   itself when `sstatus.SUM` is set; see [`crate::regs::SSTATUS_SUM`].
//! * `G` marks globally mapped pages (shared across address spaces).
//! * `A` (accessed) and `D` (dirty) are the page-tracking bits. See the
//!   note on [`pte_new`] for why VelaOS sets both at map time.
//! * Bits 62:61 are the `PBMT` field of the *Svpbmt* extension: 0 = PMA
//!   (normal memory), 1 = NC (non-cacheable), 2 = IO. VelaOS uses `IO` for
//!   [`PageFlags::mmio`] mappings and relies on the platform having Svpbmt
//!   (QEMU's virt machine does); on hardware without it the bits are reserved
//!   and must be left zero, so a build for such a platform would have to drop
//!   the encoding in [`pte_new`].
//!
//! # A/D update, and why software sets them on map
//!
//! RISC-V does not mandate that the CPU maintains `A`/`D` in hardware. When
//! hardware A/D update is disabled (the M-mode firmware controls it through
//! `menvcfg.ADUE`, and it is off by default on many platforms), an access to a
//! valid page with `A` clear — or a store to a writable page with `D` clear —
//! raises a *store/access page fault* rather than being handled silently.
//! S-mode software is therefore responsible for setting `A` (and `D` on
//! writable pages) when the mapping is created. VelaOS does exactly that in
//! [`pte_new`]: the mapped page is immediately "accessed", and writable pages
//! are immediately "dirty". This costs nothing (the bits are advisory) and
//! removes an entire class of first-touch faults.
//!
//! # PMP: the M-mode boundary
//!
//! Physical Memory Protection is *not* part of this module. The `pmpcfg*` and
//! `pmpaddr*` CSRs are only writable in M-mode (privileged spec, Vol. II
//! "Physical Memory Protection"); S-mode can read them at best and cannot
//! program them. The division of labor is therefore:
//!
//! * M-mode firmware (OpenSBI on QEMU) programs PMP entries before dropping
//!   into S-mode, protecting its own memory and granting S-mode the RAM and
//!   MMIO ranges it may use.
//! * S-mode (this crate) manages *virtual* memory through `satp` and the
//!   tables below it. A PMP violation shows up as a load/store access fault,
//!   which [`crate::idt`] reports; it is never something S-mode can fix.
//!
//! # Testing
//!
//! The walk itself is pure: [`translate`], [`map_page`], [`unmap_page`] and
//! [`set_flags`] operate through the [`PhysBackend`] trait, which reads and
//! writes 64-bit PTEs at physical addresses. The unit tests drive them with a
//! mock backend, so the three-level walk, the error cases and the flag
//! encodings are all verified on the host without any RISC-V hardware.

use vela_arch_common::{MapError, PageFlags, PAGE_SIZE};

/// Entries per table level: 512, selected by 9 index bits.
pub const ENTRIES_PER_TABLE: usize = 512;

/// Bytes occupied by one page-table level (exactly one 4 KiB frame).
pub const TABLE_BYTES: usize = ENTRIES_PER_TABLE * core::mem::size_of::<u64>();

/// `satp` mode value for Sv39 translation (privileged spec, satp.Mode field).
pub const SATP_MODE_SV39: u64 = 8;

/// The bit position of the `satp.Mode` field (bits 63:60).
pub const SATP_MODE_SHIFT: u64 = 60;

/// The bit position of the `satp.ASID` field (bits 59:44).
pub const SATP_ASID_SHIFT: u64 = 44;

/// The highest physical address Sv39 can map: the PPN field is 44 bits, so a
/// physical address at or above 2^56 cannot be encoded in a PTE.
pub const MAX_PHYSICAL: u64 = 1 << 56;

/// Where the kernel window begins: the low half of Sv39 belongs to userspace,
/// the high half (bit 38 set, sign-extended) is the kernel's.
pub const KERNEL_VIRTUAL_BASE: u64 = 0x0000_FFFF_FFC0_0000_0000;

/// True when `va` is a canonical Sv39 address: bits 63..=38 must all equal
/// bit 38. A non-canonical address never reaches the tables — the hardware
/// raises an instruction/load/store page fault first — so the kernel must
/// reject one before writing a mapping for it.
///
/// Written as "the 26 bits above the VPN2 field are all clear, or all set",
/// which is exactly the hardware's sign-extension condition.
pub const fn is_canonical(va: u64) -> bool {
    let high = va >> 38;
    high == 0 || high == 0x03FF_FFFF
}

/// True when `va` falls in the kernel's (sign-extended) high half.
pub const fn is_kernel_canonical(va: u64) -> bool {
    va >= KERNEL_VIRTUAL_BASE
}

/// Build a `satp` register value for Sv39.
///
/// The register layout (privileged spec, "satp: Supervisor Address
/// Translation and Protection Register"):
///
/// ```text
/// 63    60 59      44 43             0
/// +-------+----------+---------------+
/// | Mode  |   ASID   |      PPN      |
/// +-------+----------+---------------+
/// ```
///
/// * `Mode` 8 = Sv39 (other values: 0 = bare, 9 = Sv48).
/// * `ASID` is a 16-bit address-space identifier; VelaOS passes 0 and issues
///   a full `sfence.vma` on activation rather than tracking per-ASID TLB
///   entries.
/// * `PPN` is the physical page number of the root table — the physical
///   address with its low 12 bits dropped, because a page table must be
///   page-aligned for the entry pointing at it to be expressible.
pub const fn satp_value(ppn: u64, asid: u16) -> u64 {
    (SATP_MODE_SV39 << SATP_MODE_SHIFT)
        | ((asid as u64 & 0xFFFF) << SATP_ASID_SHIFT)
        | (ppn & 0x0000_0000_00FF_FFFF_FFFF)
}

/// The root-table page number encoded in a `satp` value.
pub const fn satp_ppn(satp: u64) -> u64 {
    satp & 0x0000_0000_00FF_FFFF_FFFF
}

/// The mode field of a `satp` value.
pub const fn satp_mode(satp: u64) -> u64 {
    (satp >> SATP_MODE_SHIFT) & 0x000F
}

/// PTE bit flags, from the privileged spec's Sv39 PTE layout.
pub mod pte_flags {
    /// Valid: the entry is in use (leaf or pointer).
    pub const VALID: u64 = 1 << 0;
    /// Readable.
    pub const READABLE: u64 = 1 << 1;
    /// Writable.
    pub const WRITABLE: u64 = 1 << 2;
    /// Executable.
    pub const EXECUTABLE: u64 = 1 << 3;
    /// User-mode accessible.
    pub const USER: u64 = 1 << 4;
    /// Global: present in every address space.
    pub const GLOBAL: u64 = 1 << 5;
    /// Accessed: set by software (see the module docs) at map time.
    pub const ACCESSED: u64 = 1 << 6;
    /// Dirty: set by software on writable pages at map time.
    pub const DIRTY: u64 = 1 << 7;
    /// Bits 9:8, reserved for supervisor software; VelaOS leaves them clear.
    pub const RSW: u64 = 0b11 << 8;
    /// The page frame number field, bits 53:10.
    ///
    /// The field holds the *page number*, not the byte address: the
    /// physical address is the page number shifted back up by twelve. The
    /// low ten bits of the word are flags, which is exactly why every
    /// table and every mapped frame must be page-aligned: the alignment
    /// *is* the flag field's zero padding.
    pub const PPN_MASK: u64 = 0x0000_000F_FFFF_FFFF_FC00;
    /// The page-aligned physical address a PPN field names.
    pub const PA_MASK: u64 = 0x0000_000F_FFFF_FFFF_F000;
    /// The `PBMT` field (Svpbmt), bits 62:61.
    pub const PBMT_MASK: u64 = 0b11 << 61;

    /// `PBMT` encoding: page-based memory attributes as in the PMA.
    pub const PBMT_PMA: u64 = 0b00 << 61;
    /// `PBMT` encoding: non-cacheable, weakly ordered.
    pub const PBMT_NC: u64 = 0b01 << 61;
    /// `PBMT` encoding: device I/O, strongly ordered.
    pub const PBMT_IO: u64 = 0b10 << 61;
}

/// True when a PTE describes a *leaf* mapping (any of R, W, X set) at its
/// level, rather than a pointer to the next table.
pub const fn pte_is_leaf(pte: u64) -> bool {
    pte & (pte_flags::READABLE | pte_flags::WRITABLE | pte_flags::EXECUTABLE) != 0
}

/// True when the PTE is valid — the only bit the hardware demands before it
/// will use an entry at all.
pub const fn pte_valid(pte: u64) -> bool {
    pte & pte_flags::VALID != 0
}

/// The physical page number encoded in a leaf PTE, shifted into a physical
/// address. For a non-leaf PTE this is the address of the next table.
pub const fn pte_pa(pte: u64) -> u64 {
    pte_ppn(pte) << 12
}

/// The page frame number (bits 53:10) of a PTE.
pub const fn pte_ppn(pte: u64) -> u64 {
    (pte & pte_flags::PPN_MASK) >> 10
}

/// Build a *non-leaf* PTE pointing at the next table.
///
/// Only `V` is set. `R`/`W`/`X` must be clear for a pointer entry (any of
/// them set would make the hardware treat the table itself as a huge leaf
/// mapping), and the table address must be page-aligned because its low 12
/// bits are consumed by the PPN field.
pub const fn pte_new_table(table_pa: u64) -> u64 {
    pte_flags::VALID | ((table_pa & pte_flags::PA_MASK) >> 2)
}

/// Build a *leaf* PTE mapping `pa` with `flags`.
///
/// # The A/D decision
///
/// `A` is set for every mapping and `D` additionally for writable ones. See
/// the module docs: RISC-V leaves A/D maintenance optional in hardware, so the
/// software that creates the mapping sets them, or the first instruction that
/// touches the page faults instead of running. The bits are advisory (they
/// exist so a paging daemon can evict pages), and pre-setting them simply
/// declares the page "touched" — correct for a v0.1 kernel with no eviction.
///
/// # The R-before-W rule
///
/// A PTE with W set and R clear is a reserved encoding (the spec reserves it
/// "for future use"), and reserved encodings *fault* on access. Every
/// writable page is therefore made readable too. [`PageFlags`] never expresses
/// write-without-read, but the rule is enforced here so a hand-built flags
/// value cannot sneak one into the tables.
///
/// `is_leaf` distinguishes the two PTE shapes: a leaf carries R/W/X/U and the
/// A/D bits; a table pointer carries only `V` (see [`pte_new_table`]).
pub const fn pte_new(phys: u64, flags: PageFlags, is_leaf: bool) -> u64 {
    if !is_leaf {
        return pte_new_table(phys);
    }
    // W without R is a reserved encoding; make writable pages readable.
    let readable = flags.readable || flags.writable;
    let mut out = pte_flags::VALID | pte_flags::ACCESSED | ((phys & pte_flags::PA_MASK) >> 2);
    if readable {
        out |= pte_flags::READABLE;
    }
    if flags.writable {
        out |= pte_flags::WRITABLE | pte_flags::DIRTY;
    }
    if flags.executable {
        out |= pte_flags::EXECUTABLE;
    }
    if flags.user {
        out |= pte_flags::USER;
    }
    if flags.uncacheable {
        // Device memory: strongly ordered, non-cacheable, per Svpbmt.
        out |= pte_flags::PBMT_IO;
    }
    // `G` is deliberately not set: v0.1 flushes the whole TLB on activation,
    // so global pages would buy nothing and add a live/evict subtlety.
    out
}

/// The page flags of a valid leaf PTE, for diagnostics and tests.
///
/// `readable` reports the R bit as stored — not the "writable implies
/// readable" promotion [`pte_new`] applies, which is why a PTE built from
/// `PageFlags` with `writable` but no `readable` does not round-trip
/// bit-for-bit (and why that request is refused at the encoding, not the
/// decode).
pub const fn pte_flags_of(pte: u64) -> PageFlags {
    PageFlags {
        readable: pte & pte_flags::READABLE != 0,
        writable: pte & pte_flags::WRITABLE != 0,
        executable: pte & pte_flags::EXECUTABLE != 0,
        user: pte & pte_flags::USER != 0,
        // RISC-V has no separate user-writable encoding: a user page is
        // writable exactly when the leaf is writable *and* carries U.
        user_writable: pte & (pte_flags::USER | pte_flags::WRITABLE)
            == (pte_flags::USER | pte_flags::WRITABLE),
        uncacheable: pte & pte_flags::PBMT_MASK == pte_flags::PBMT_IO,
    }
}

/// A decomposed Sv39 virtual address.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Split {
    /// Root table index (virtual address bits 38:30).
    pub vpn2: usize,
    /// Middle table index (bits 29:21).
    pub vpn1: usize,
    /// Leaf table index (bits 20:12).
    pub vpn0: usize,
    /// Byte offset within the 4 KiB page.
    pub offset: u64,
}

impl Split {
    /// Decompose `va`.
    pub const fn of(va: u64) -> Self {
        Self {
            vpn2: ((va >> 30) & 0x01FF) as usize,
            vpn1: ((va >> 21) & 0x01FF) as usize,
            vpn0: ((va >> 12) & 0x01FF) as usize,
            offset: va & 0x0FFF,
        }
    }

    /// Reassemble the page-aligned base of this split. The high bits are
    /// re-derived by sign extension of bit 38, exactly as the hardware
    /// interprets them, so `base_of() + offset` round-trips every canonical
    /// address.
    pub const fn base_of(&self) -> u64 {
        let low =
            ((self.vpn2 as u64) << 30) | ((self.vpn1 as u64) << 21) | ((self.vpn0 as u64) << 12);
        // Sign-extend bit 38 into bits 63:39.
        if low & (1 << 38) != 0 {
            low | (!0u64 << 39)
        } else {
            low
        }
    }

    /// The three indices in root-to-leaf order.
    pub const fn path(&self) -> [usize; 3] {
        [self.vpn2, self.vpn1, self.vpn0]
    }
}

/// Read/write access to page-table memory by *physical* address.
///
/// The walk code in this module never dereferences pointers itself: it asks a
/// backend to read or write one 64-bit PTE at a physical address, and to hand
/// out a zeroed, page-aligned frame for a new table. On the target the
/// concrete backend is supplied by the kernel (whose heap is identity-mapped
/// during boot, so a table's physical address is the pointer to it — see
/// [`crate::addrspace`]); on the host the tests supply a mock, which is what
/// makes the whole walk unit-testable.
pub trait PhysBackend {
    /// Read the 64-bit PTE stored at physical address `pa`.
    ///
    /// # Errors
    ///
    /// [`MapError::NotMapped`] when the backend cannot service the address —
    /// for the heap pool this means the caller named a table that was never
    /// allocated, which is a kernel bug and not a user-visible condition.
    fn read_pte(&mut self, pa: u64) -> Result<u64, MapError>;

    /// Write a 64-bit PTE at physical address `pa`.
    ///
    /// # Errors
    ///
    /// [`MapError::NotMapped`] when no table backs `pa`, as in [`Self::read_pte`].
    fn write_pte(&mut self, pa: u64, value: u64) -> Result<(), MapError>;

    /// Allocate a zeroed, page-aligned frame for a new table and return its
    /// physical address.
    ///
    /// # Errors
    ///
    /// [`MapError::OutOfMemory`] when the platform has no frame left.
    fn alloc_table(&mut self) -> Result<u64, MapError>;
}

/// The PTE address of entry `index` of the table at `table_pa`.
///
/// Exposed because both the walk and the address-space deep-clone need the
/// same arithmetic, and because it is the one place the "PTEs are 8 bytes,
/// tables are 4 KiB" invariant lives.
pub const fn pte_address(table_pa: u64, index: usize) -> u64 {
    table_pa + (index as u64) * 8
}

/// Translate `va` in the space rooted at `root_pa`.
///
/// The walk mirrors the hardware's three levels exactly: read the root entry
/// selected by VPN2, descend if it is a pointer, then VPN1, then VPN0, and
/// return the leaf's physical address plus the page offset. A leaf found at
/// VPN2 (a 1 GiB megapage) or VPN1 (a 2 MiB megapage) is honoured by adding
/// the offset masked to that level's span, which is what the hardware does.
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
    // Levels 2 and 1: descend through pointer entries, honouring megapages.
    for (level, index) in [(2usize, split.vpn2), (1, split.vpn1)] {
        let pte = backend.read_pte(pte_address(table_pa, index))?;
        if !pte_valid(pte) {
            return Ok(None);
        }
        if pte_is_leaf(pte) {
            // A megapage: the entry's PPN is aligned to this level's span,
            // and the low bits of `va` name the byte within it.
            let span = 1u64 << (12 + 9 * level as u64);
            return Ok(Some(pte_pa(pte) + (va & (span - 1))));
        }
        table_pa = pte_pa(pte);
    }
    let pte = backend.read_pte(pte_address(table_pa, split.vpn0))?;
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
/// * [`MapError::AddressOverflow`] — `pa` is outside the 2^56 range a Sv39
///   PPN can encode.
/// * [`MapError::AlreadyMapped`] — the slot holds a different mapping.
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
    for index in [split.vpn2, split.vpn1] {
        let slot = pte_address(table_pa, index);
        let pte = backend.read_pte(slot)?;
        if !pte_valid(pte) {
            let child = backend.alloc_table()?;
            backend.write_pte(slot, pte_new_table(child))?;
            table_pa = child;
        } else if pte_is_leaf(pte) {
            // A megapage already covers this range; the caller asked for a
            // 4 KiB mapping under it, which would conflict.
            return Err(MapError::AlreadyMapped);
        } else {
            table_pa = pte_pa(pte);
        }
    }
    let slot = pte_address(table_pa, split.vpn0);
    let existing = backend.read_pte(slot)?;
    if pte_valid(existing) && pte_pa(existing) != (pa & pte_flags::PA_MASK) {
        return Err(MapError::AlreadyMapped);
    }
    backend.write_pte(slot, pte_new(pa, flags, true))
}

/// Remove the mapping at `va`, returning the physical frame it held.
///
/// The intermediate tables above the leaf are *not* returned to the backend:
/// like the x86-64 implementation, unmapping a page leaves its (possibly
/// shared) tables in place, and only the leaf frame goes back to the caller.
///
/// # Errors
///
/// [`MapError::NotMapped`] when `va` has no valid leaf, or
/// [`MapError::MisalignedVirtual`] when `va` is not page-aligned.
pub fn unmap_page(backend: &mut dyn PhysBackend, root_pa: u64, va: u64) -> Result<u64, MapError> {
    if !va.is_multiple_of(PAGE_SIZE) {
        return Err(MapError::MisalignedVirtual);
    }
    let split = Split::of(va);
    let mut table_pa = root_pa;
    for index in [split.vpn2, split.vpn1] {
        let pte = backend.read_pte(pte_address(table_pa, index))?;
        if !pte_valid(pte) || pte_is_leaf(pte) {
            // No path, or a megapage where this walker expects a pointer:
            // either way there is no 4 KiB leaf below.
            return Err(MapError::NotMapped);
        }
        table_pa = pte_pa(pte);
    }
    let slot = pte_address(table_pa, split.vpn0);
    let existing = backend.read_pte(slot)?;
    if !pte_valid(existing) {
        return Err(MapError::NotMapped);
    }
    backend.write_pte(slot, 0)?;
    Ok(pte_pa(existing))
}

/// Change the flags of an existing mapping without moving its frame.
///
/// # Errors
///
/// [`MapError::NotMapped`] when `va` has no valid leaf, or
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
    let split = Split::of(va);
    let mut table_pa = root_pa;
    for index in [split.vpn2, split.vpn1] {
        let pte = backend.read_pte(pte_address(table_pa, index))?;
        if !pte_valid(pte) || pte_is_leaf(pte) {
            return Err(MapError::NotMapped);
        }
        table_pa = pte_pa(pte);
    }
    let slot = pte_address(table_pa, split.vpn0);
    let existing = backend.read_pte(slot)?;
    if !pte_valid(existing) {
        return Err(MapError::NotMapped);
    }
    backend.write_pte(slot, pte_new(pte_pa(existing), flags, true))
}

#[cfg(test)]
extern crate std;

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::vec::Vec;

    /// A mock [`PhysBackend`]: a map of PTE slots plus a bump allocator for
    /// table frames. Every walk in the tests runs against this.
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
    fn satp_layout_matches_the_spec() {
        // Mode 8 in bits 63:60, ASID in 59:44, PPN in 43:0.
        let satp = satp_value(0x0001_2345, 0xBEEF);
        assert_eq!(satp_mode(satp), SATP_MODE_SV39);
        assert_eq!((satp >> SATP_ASID_SHIFT) & 0xFFFF, 0xBEEF);
        assert_eq!(satp_ppn(satp), 0x0001_2345);
        // Mode and ASID must not collide: the ASID's top bit sits at 59,
        // safely below the mode field.
        assert_eq!((satp_value(0, 0xFFFF) >> 60) & 0x000F, SATP_MODE_SV39);
    }

    #[test]
    fn satp_of_page_aligned_root_is_just_mode_plus_ppn() {
        let satp = satp_value(0x8000, 0);
        assert_eq!(satp, (SATP_MODE_SV39 << SATP_MODE_SHIFT) | 0x8000);
    }

    #[test]
    fn pte_round_trips_per_flags_variant() {
        // Each variant maps to the documented bit pattern. W implies R (the
        // reserved W-without-R encoding is refused), so every writable
        // variant is readable as well.
        let cases: [(PageFlags, u64, u64, u64, bool); 6] = [
            (PageFlags::kernel_rw(), 1, 1, 0, false),
            (PageFlags::kernel_rx(), 1, 0, 1, false),
            (PageFlags::kernel_ro(), 1, 0, 0, false),
            (PageFlags::user_rw(), 1, 1, 0, true),
            (PageFlags::user_rx(), 1, 0, 1, true),
            (PageFlags::mmio(), 1, 1, 0, false),
        ];
        for (flags, r, w, x, user) in cases {
            let pte = pte_new(0x0DEA_0000, flags, true);
            assert!(pte_valid(pte), "every map is valid");
            assert_eq!(pte & pte_flags::READABLE != 0, r != 0);
            assert_eq!(pte & pte_flags::WRITABLE != 0, w != 0);
            assert_eq!(pte & pte_flags::EXECUTABLE != 0, x != 0);
            assert_eq!(pte & pte_flags::USER != 0, user);
            // A/D are software-set at map time; see the module docs.
            assert_ne!(pte & pte_flags::ACCESSED, 0);
            assert_eq!(
                pte & pte_flags::DIRTY != 0,
                flags.writable,
                "D tracks writability"
            );
            assert_eq!(pte & pte_flags::GLOBAL, 0, "G is not set in v0.1");
            assert_eq!(pte_pa(pte), 0x0DEA_0000);
        }
    }

    #[test]
    fn mmio_sets_the_io_memory_type() {
        let pte = pte_new(0x0000_1000_1000, PageFlags::mmio(), true);
        assert_eq!(pte & pte_flags::PBMT_MASK, pte_flags::PBMT_IO);
        assert!(pte_flags_of(pte).uncacheable);
        // Ordinary RAM pages carry no PBMT override.
        let pte = pte_new(0x1000, PageFlags::kernel_rw(), true);
        assert_eq!(pte & pte_flags::PBMT_MASK, pte_flags::PBMT_PMA);
        assert!(!pte_flags_of(pte).uncacheable);
    }

    #[test]
    fn user_pages_set_the_user_bit() {
        assert_ne!(
            pte_new(0x2000, PageFlags::user_rx(), true) & pte_flags::USER,
            0
        );
        assert_eq!(
            pte_new(0x2000, PageFlags::kernel_rw(), true) & pte_flags::USER,
            0
        );
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
            let pte = pte_new(0x3000, flags, true);
            let back = pte_flags_of(pte);
            assert_eq!(back.readable, flags.readable || flags.writable);
            assert_eq!(back.writable, flags.writable);
            assert_eq!(back.executable, flags.executable);
            assert_eq!(back.user, flags.user);
            assert_eq!(back.user_writable, flags.user_writable);
            assert_eq!(back.uncacheable, flags.uncacheable);
        }
    }

    #[test]
    fn writable_without_readable_is_promoted_not_encoded() {
        let bad = PageFlags {
            readable: false,
            writable: true,
            ..PageFlags::kernel_rw()
        };
        let pte = pte_new(0x4000, bad, true);
        // The reserved W-only encoding must never be produced.
        assert_eq!(pte & pte_flags::READABLE, pte_flags::READABLE);
        assert_eq!(pte & pte_flags::WRITABLE, pte_flags::WRITABLE);
    }

    #[test]
    fn table_pointers_carry_only_the_valid_bit() {
        let pte = pte_new_table(0x5000);
        assert!(pte_valid(pte));
        assert!(!pte_is_leaf(pte), "a pointer has no R/W/X bits");
        assert_eq!(pte_pa(pte), 0x5000);
        // Flags and address never collide: the low 12 bits of the table
        // address are consumed by the flag field.
        assert_eq!(pte_pa(pte_new_table(0x5001)), 0x5000);
    }

    #[test]
    fn split_round_trips() {
        for va in [
            0u64,
            0x1000,
            0x0000_FFFF_FFC0_0000_1000,
            KERNEL_VIRTUAL_BASE,
            KERNEL_VIRTUAL_BASE + 0x0123_4567,
            0x0000_FFFF_FFFF_FFFF_F000,
        ] {
            assert!(is_canonical(va), "test input must be canonical");
            let s = Split::of(va);
            assert_eq!(s.base_of() + s.offset, va, "round-trip failed for {va:#x}");
        }
    }

    #[test]
    fn non_canonical_addresses_are_recognised() {
        // Bit 38 set without the sign extension: the hardware page-faults on
        // these before ever reading a table.
        assert!(!is_canonical(0x0000_0000_0080_0000_0000));
        assert!(is_canonical(0x0000_0000_0000_4000_0000));
        assert!(is_canonical(0x0000_FFFF_FFFF_C000_0000));
        assert!(!is_canonical(0x0000_0000_8000_0000_0000));
    }

    #[test]
    fn split_indices_are_in_range() {
        for i in 0..4096u64 {
            let s = Split::of(i.wrapping_mul(0x0000_1234_5678_9ABC).wrapping_mul(0x001F));
            for idx in s.path() {
                assert!(idx < ENTRIES_PER_TABLE);
            }
            assert!(s.offset < PAGE_SIZE);
        }
    }

    #[test]
    fn map_translate_unmap_walk() {
        let (mut mem, root) = root_backend();
        let va = 0x0000_0000_1234_5000;
        let pa = 0x0000_0000_0ABC_D000;

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
    fn remapping_the_same_frame_updates_flags() {
        let (mut mem, root) = root_backend();
        let va = 0x2000;
        map_page(&mut mem, root, va, 0x7000, PageFlags::user_rw()).expect("map");
        // Same frame, different flags: a flags update, not a conflict.
        map_page(&mut mem, root, va, 0x7000, PageFlags::kernel_ro()).expect("remap");
        // The walk is computed first: nesting the reads inside the outer
        // call would borrow the backend twice at once.
        let mut table = root;
        for idx in [Split::of(va).vpn2, Split::of(va).vpn1] {
            table = pte_pa(mem.read_pte(pte_address(table, idx)).unwrap());
        }
        let pte = mem
            .read_pte(pte_address(table, Split::of(va).vpn0))
            .unwrap();
        assert!(!pte_flags_of(pte).user, "flags were re-encoded");
        assert!(pte_flags_of(pte).readable);
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
    fn misaligned_and_overflow_errors_match_the_x86_surface() {
        let (mut mem, root) = root_backend();
        assert_eq!(
            map_page(&mut mem, root, 0x1800, 0x1000, PageFlags::kernel_rw()),
            Err(MapError::MisalignedVirtual)
        );
        assert_eq!(
            map_page(&mut mem, root, 0x1000, 0x1800, PageFlags::kernel_rw()),
            Err(MapError::MisalignedPhysical)
        );
        // Above 2^56: a Sv39 PPN cannot encode it.
        assert_eq!(
            map_page(&mut mem, root, 0x1000, MAX_PHYSICAL, PageFlags::kernel_rw()),
            Err(MapError::AddressOverflow)
        );
        // Non-canonical virtual address.
        assert_eq!(
            map_page(
                &mut mem,
                root,
                0x0000_0000_0080_0000_0000,
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
    fn two_pages_share_one_leaf_table() {
        let (mut mem, root) = root_backend();
        map_page(&mut mem, root, 0x7000, 0xB000, PageFlags::kernel_rw()).expect("first");
        map_page(&mut mem, root, 0x8000, 0xC000, PageFlags::kernel_rw()).expect("second");
        // Three levels means exactly three tables: root, middle, leaf.
        assert_eq!(mem.allocated_tables(), 3);
        assert_eq!(translate(&mut mem, root, 0x7000).unwrap(), Some(0xB000));
        assert_eq!(translate(&mut mem, root, 0x8000).unwrap(), Some(0xC000));
    }

    #[test]
    fn megapage_leaves_are_honoured_by_translate() {
        // A 1 GiB leaf at the root level: R/W/X set, PPN aligned to 1 GiB.
        let (mut mem, root) = root_backend();
        let one_gib = 1024 * 1024 * 1024;
        let megapage = pte_flags::VALID
            | pte_flags::READABLE
            | pte_flags::WRITABLE
            | pte_flags::ACCESSED
            | pte_flags::DIRTY
            | ((one_gib & pte_flags::PA_MASK) >> 2);
        mem.slots
            .insert(pte_address(root, Split::of(one_gib).vpn2), megapage);
        assert_eq!(
            translate(&mut mem, root, one_gib + 0x1234).unwrap(),
            Some(one_gib + 0x1234)
        );
    }

    #[test]
    fn table_geometry_is_consistent() {
        assert_eq!(TABLE_BYTES, 4096);
        assert_eq!(ENTRIES_PER_TABLE * 8, 4096);
        assert_eq!(pte_address(0x1000, 511), 0x1000 + 511 * 8);
        assert_eq!(pte_address(0x1000, 512), 0x2000, "next table starts here");
    }

    #[test]
    fn pte_ppn_extracts_the_page_number() {
        let pte = pte_new(0x00AB_C000, PageFlags::kernel_rw(), true);
        assert_eq!(pte_ppn(pte), 0x00AB_C000 >> 12);
        assert_eq!(pte_pa(pte), 0x00AB_C000);
    }
}
