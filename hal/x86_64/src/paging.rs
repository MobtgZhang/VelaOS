//! Four-level x86-64 page tables.
//!
//! # Structure
//!
//! A 48-bit virtual address decomposes as
//!
//! ```text
//! 63    48 47   39 38    30 29    21 20    12 11     0
//! +-------+-------+--------+--------+--------+---------+
//! | sign  | PML4  |  PDPT  |   PD   |   PT   |  offset |
//! | ext.  | index | index  | index  | index  |         |
//! +-------+-------+--------+--------+--------+---------+
//! ```
//!
//! Each level is an array of 512 8-byte entries indexed by 9 bits. Three
//! consecutive 1-bits in an entry mean "this entry is a leaf at this level",
//! which is how a 4 KiB page, a 2 MiB page and a 1 GiB page are encoded.
//!
//! # Safety model
//!
//! Two rules keep this sound, and both are enforced by construction rather
//! than by convention:
//!
//! 1. **Address arithmetic is pure and safe.** [`VirtAddr::split`] and friends
//!    are ordinary functions. Only the functions that actually write an entry
//!    take `&mut` to an [`AddressSpace`], so no code path can produce a table
//!    entry without an owner to attribute it to.
//! 2. **Frame ownership is tracked.** A physical frame may be present in at
//!    most one page-table level of one address space, so [`AddressSpace::unmap`]
//!    returns exactly the frame it removed and the caller must hand it back to
//!    the allocator. Freeing a table without freeing its children is
//!    [`AddressSpace::free_tables`], which walks bottom-up.

use core::arch::asm;
use core::mem::size_of;

use vela_hal_common::PageFlags;

// Only the test module needs these; the paging core itself never names
// `MapError` or `PAGE_SIZE`, so importing them at module scope would warn on
// the bare-metal build where `#[cfg(test)]` is compiled out.
#[cfg(test)]
use vela_hal_common::{MapError, PAGE_SIZE};

/// Entries per page-table level: 512, selected by 9 index bits.
pub const ENTRIES_PER_TABLE: usize = 512;

/// Bytes occupied by one page-table level.
pub const TABLE_BYTES: usize = ENTRIES_PER_TABLE * size_of::<u64>();

/// Bytes spanned by one page-table entry at each level.
pub const fn bytes_covered(level: u8) -> u64 {
    1 << (12 + 9 * level as u32)
}

/// Page-table levels, deepest first: page table, page directory, PDPT, PML4.
pub const LEVELS: [PageLevel; 4] = [
    PageLevel::PageTable,
    PageLevel::PageDirectory,
    PageLevel::PageTableDirectoryPointer,
    PageLevel::PageMapLevel4,
];

/// The levels a walk visits **between** the root and the leaf table.
///
/// # Why this is not just `LEVELS`
///
/// The root of an address space *is* the PML4: it is the table whose entries
/// point at the PDPTs. A walk therefore visits three levels — PML4, PDPT, page
/// directory — and lands on the **page table**, whose entries are the leaves
/// the hardware finally reads.
///
/// Visiting four levels instead would descend one level *past* the page
/// table, into a table the hardware never consults. The leaves would then be
/// written into a table that sits below the page table, and the page table's
/// own entry would carry a *table pointer* where the CPU expects a leaf
/// page-table entry. The hardware ignores the software-only `AVAILABLE` bit,
/// so it would interpret that pointer as a mapping of the page table's own
/// physical frame — and every access through the space would read the wrong
/// bytes, at the wrong offset.
///
/// The failure is silent and total: the mapping code reports success, the
/// tables look correct to software, and the CPU disagrees.
pub const INTERMEDIATE_LEVELS: [PageLevel; 3] = [
    PageLevel::PageMapLevel4,
    PageLevel::PageTableDirectoryPointer,
    PageLevel::PageDirectory,
];

/// Which of the four tables an entry lives in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum PageLevel {
    /// The leaf table: 512 entries, each one 4 KiB. Index bits 20..=29.
    PageTable = 0,
    /// 512 entries, each covering 2 MiB. Index bits 29..=38.
    PageDirectory = 1,
    /// 512 entries, each covering 1 GiB. Index bits 38..=47.
    PageTableDirectoryPointer = 2,
    /// The root: 512 entries, each covering 512 GiB. Index bits 47..=56.
    PageMapLevel4 = 3,
}

impl PageLevel {
    /// The 9 index bits at this level.
    pub const fn index(self, va: u64) -> usize {
        ((va >> (12 + 9 * self as u32)) & 0x1FF) as usize
    }

    /// The level one step toward the root.
    pub const fn parent(self) -> Option<Self> {
        match self {
            Self::PageMapLevel4 => None,
            // Ordered so each arm is a single mapping and the compiler can see
            // the match is total without a catch-all `unreachable!`, which is
            // not allowed in a `const fn`.
            Self::PageTableDirectoryPointer => Some(Self::PageMapLevel4),
            Self::PageDirectory => Some(Self::PageTableDirectoryPointer),
            Self::PageTable => Some(Self::PageDirectory),
        }
    }

    /// The level one step toward the leaves.
    pub const fn child(self) -> Option<Self> {
        match self {
            Self::PageMapLevel4 => Some(Self::PageTableDirectoryPointer),
            Self::PageTableDirectoryPointer => Some(Self::PageDirectory),
            Self::PageDirectory => Some(Self::PageTable),
            Self::PageTable => None,
        }
    }
}

/// True when `va` is a canonical x86-64 address.
///
/// Canonical means the 48-bit address is either in the low half (bit 47 clear
/// and bits 63..=48 clear) or the high half (bit 47 set and bits 63..=48 all
/// ones). A non-canonical address — bit 47 set but the high bits *not* a sign
/// extension, such as `0x0000_8000_0000_0000` — raises a general protection
/// fault, so the kernel must reject one before it ever reaches a page-table
/// walk.
///
/// The rule is written as "the high 16 bits equal the sign extension of bit
/// 47", which is exactly the hardware's condition and covers both halves in a
/// single expression. Checking only that the high bits are all-zero-or-all-one
/// is *not* sufficient: `0x0000_8000_0000_0000` passes that weaker test and
/// still faults.
pub const fn is_canonical(va: u64) -> bool {
    let sign = if va & (1u64 << 47) != 0 { 0xFFFFu64 } else { 0 };
    (va >> 48) == sign
}

/// A decomposed virtual address.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Split {
    /// PML4 index.
    pub pml4: usize,
    /// PDPT index.
    pub pdpt: usize,
    /// Page-directory index.
    pub pd: usize,
    /// Page-table index.
    pub pt: usize,
    /// Byte offset within the 4 KiB page.
    pub offset: u64,
}

impl Split {
    /// Decompose `va`.
    pub const fn of(va: u64) -> Self {
        Self {
            pml4: ((va >> 39) & 0x1FF) as usize,
            pdpt: ((va >> 30) & 0x1FF) as usize,
            pd: ((va >> 21) & 0x1FF) as usize,
            pt: ((va >> 12) & 0x1FF) as usize,
            offset: va & 0xFFF,
        }
    }

    /// Reassemble a page-aligned virtual address, discarding the offset.
    ///
    /// The four indices cover only the low 48 bits, so the high half of the
    /// address has to be re-derived. A canonical high address has bit 47 set,
    /// and the CPU requires bits 63..=48 to be a *sign extension* of bit 47 —
    /// so the reconstruction is `bits | (!0 << 48)` when bit 47 is set, and
    /// plain `bits` when it is not. Getting this wrong is not cosmetic: it
    /// turns a kernel-half address into a completely different low address.
    ///
    /// A *non-canonical* input (bit 47 set but the high bits not a sign
    /// extension) cannot be reconstructed, because the decomposition loses the
    /// information that distinguishes `0x0000_8000_0000_0000` from
    /// `0xFFFF_8000_0000_0000`. Such addresses fault on the CPU, so the
    /// kernel never has to reason about them — but [`Self::base_of`] documents
    /// the limitation rather than pretending it does not exist.
    pub const fn base_of(&self) -> u64 {
        let low = ((self.pml4 as u64) << 39)
            | ((self.pdpt as u64) << 30)
            | ((self.pd as u64) << 21)
            | ((self.pt as u64) << 12);
        // Bit 47 is the top bit of the 48-bit address.
        if low & (1u64 << 47) != 0 {
            low | (0xFFFF_0000_0000_0000u64)
        } else {
            low
        }
    }

    /// The nine indices in root-to-leaf order.
    pub const fn path(&self) -> [usize; 4] {
        [self.pml4, self.pdpt, self.pd, self.pt]
    }
}

/// Entry bit flags.
pub mod entry_flags {
    /// Entry is valid.
    pub const PRESENT: u64 = 1 << 0;
    /// Entry is writable.
    pub const WRITABLE: u64 = 1 << 1;
    /// Entry is accessible from ring 3.
    pub const USER: u64 = 1 << 2;
    /// Write-through caching.
    pub const WRITE_THROUGH: u64 = 1 << 3;
    /// Cache disabled.
    pub const NO_CACHE: u64 = 1 << 4;
    /// Accessed: the hardware set this bit.
    pub const ACCESSED: u64 = 1 << 5;
    /// Dirty: the hardware set this bit when written through this entry.
    pub const DIRTY: u64 = 1 << 6;
    /// Must be set on every leaf entry when `CR4.PSE` is set.
    pub const PSE: u64 = 1 << 7;
    /// Global: present in all address spaces that share the PML4's kernel part.
    pub const GLOBAL: u64 = 1 << 8;
    /// Available to software; hardware ignores it.
    pub const AVAILABLE: u64 = 1 << 9;
    /// No-execute.
    pub const NX: u64 = 1 << 63;

    /// Bits the CPU may write: hardware-managed, software must preserve.
    pub const HW_MANAGED: u64 = ACCESSED | DIRTY;
    /// Bits that describe a leaf's address rather than its properties.
    pub const ADDRESS_MASK: u64 = 0x000F_FFFF_FFFF_F000;
}

/// The raw 64-bit content of a page-table entry.
///
/// Kept as a newtype so that a bare `u64` cannot be passed where an entry is
/// expected by accident, which is the most likely paging bug.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(transparent)]
pub struct PageTableEntry(pub u64);

impl PageTableEntry {
    /// An entry that is absent.
    pub const fn empty() -> Self {
        Self(0)
    }

    /// True when the entry points at something.
    pub const fn is_present(self) -> bool {
        self.0 & entry_flags::PRESENT != 0
    }

    /// True when this entry points at a **lower table** rather than at memory.
    ///
    /// Detected with `AVAILABLE` (bit 9), which is reserved for software and
    /// ignored by the CPU. Every entry VelaOS writes for an intermediate level
    /// sets it; no leaf entry ever does. The architectural alternative is
    /// inspecting `PS`, which is ambiguous: `PS` is only meaningful at the PDPT
    /// and PD levels and is *ignored* at the PML4 and PT levels, so a bit-test
    /// cannot distinguish "table pointer" from "leaf" uniformly across all four.
    /// An explicit software marker removes the ambiguity at zero cost, since the
    /// hardware ignores the bit.
    pub const fn is_table_pointer(self) -> bool {
        self.is_present() && self.0 & entry_flags::AVAILABLE != 0
    }

    /// True when this entry maps memory rather than pointing at a table.
    pub const fn is_leaf(self) -> bool {
        self.is_present() && !self.is_table_pointer()
    }

    /// True when `PS` is set — a large page.
    ///
    /// Only meaningful at the PDPT and PD levels; see [`Self::is_table_pointer`].
    pub const fn is_large(self) -> bool {
        self.0 & entry_flags::PSE != 0
    }

    /// The physical address this entry targets, with the flags masked off.
    pub const fn physical(self) -> u64 {
        self.0 & entry_flags::ADDRESS_MASK
    }

    /// True when the entry is present and not writable.
    pub const fn is_readonly(self) -> bool {
        self.is_present() && self.0 & entry_flags::WRITABLE == 0
    }

    /// Build a 4 KiB leaf entry for `pa`.
    pub const fn new_4kib(pa: u64, flags: PageFlags) -> Self {
        // `AVAILABLE` is deliberately *not* set: it is this crate's marker for
        // "this entry points at a lower table", and a leaf must not carry it.
        Self((pa & entry_flags::ADDRESS_MASK) | encode_flags(flags))
    }

    /// Build a leaf entry at the given level, for a large page.
    ///
    /// `pa` must already be aligned to the span this level covers; the caller
    /// is responsible for that because only it knows the intended layout.
    pub const fn new_large(pa: u64, flags: PageFlags) -> Self {
        Self((pa & entry_flags::ADDRESS_MASK) | entry_flags::PSE | encode_flags(flags))
    }

    /// Read an entry.
    ///
    /// # Safety
    ///
    /// `slot` must be a live page-table entry: either a physical frame mapped
    /// into the kernel's address space, or a pointer into a table owned by
    /// [`AddressSpace`].
    pub unsafe fn read(slot: *const Self) -> Self {
        // SAFETY: the caller guarantees `slot` is a valid, aligned entry.
        unsafe { core::ptr::read_volatile(slot) }
    }

    /// Write an entry.
    ///
    /// # Safety
    ///
    /// As [`Self::read`]; additionally the write must be visible to the CPU, so
    /// the caller is responsible for the matching [`flush`] or `invlpg`.
    pub unsafe fn write(slot: *mut Self, entry: Self) {
        // SAFETY: the caller guarantees `slot` is a valid, aligned entry.
        unsafe { core::ptr::write_volatile(slot, entry) }
    }
}

/// Translate [`PageFlags`] into entry bits.
///
/// # NX polarity
///
/// `NX` is "No-Execute", so **setting it forbids execution** and clearing it
/// permits it. This trips people up because the name reads like a feature flag
/// rather than a prohibition, so it is stated here and asserted in the tests
/// below. A page is executable exactly when its `NX` bit is *clear*.
///
/// # W^X
///
/// W^X is enforced here rather than at each call site: a flags value asking for
/// both writable and executable resolves to **non-executable**, keeping the
/// write permission the caller explicitly asked for. The reasoning is that a
/// silent privilege escalation to executable memory is unrecoverable once a
/// gadget has been written, whereas a missing execute permission is a visible,
/// debuggable failure at the first instruction fetch.
pub const fn encode_flags(flags: PageFlags) -> u64 {
    let mut out = entry_flags::PRESENT;
    if flags.writable {
        out |= entry_flags::WRITABLE;
    }
    if flags.user {
        out |= entry_flags::USER;
    }
    if flags.uncacheable {
        out |= entry_flags::NO_CACHE | entry_flags::WRITE_THROUGH;
    }
    // `user_writable` is meaningless without `user`, and is not silently
    // promoted: the caller must ask for both.
    let executable = flags.executable && !flags.writable;
    if !executable {
        // `NX` set = forbidden. Every page VelaOS maps is NX by default, so
        // execute permission is something a caller must positively earn.
        out |= entry_flags::NX;
    }
    out
}

/// True when an encoded entry permits instruction fetch.
///
/// The single place the NX polarity is expressed for readers of encoded
/// entries, so no other code has to remember which way the bit runs.
pub const fn encoded_allows_execute(encoded: u64) -> bool {
    // NX set means "no execute", so executable means the bit is clear.
    // `is_leaf` is also required: an intermediate pointer is not fetchable even
    // with NX clear, which is what stops a crafted page table from turning a
    // table into a gadget.
    (encoded & entry_flags::NX == 0)
        && (encoded & (entry_flags::WRITABLE | entry_flags::PSE)
            != entry_flags::WRITABLE | entry_flags::PSE)
}

/// Whether this build runs on bare metal with real page tables.
///
/// `#![cfg(target_arch = "x86_64")]` is true on both Linux and bare metal, so
/// architecture alone does not answer the question. What differs is whether
/// the CPU is running in ring 0 with a page table it owns, and only the target
/// triple knows that. `no_std` crates compile for both, so the flag has to be a
/// real `cfg` rather than a runtime check.
#[cfg(target_os = "none")]
const ON_BARE_METAL: bool = true;
#[cfg(not(target_os = "none"))]
const ON_BARE_METAL: bool = false;

/// Whether privileged TLB instructions may be issued.
///
/// Exposed so [`crate::addrspace::activate`] can apply the same rule to `CR3`.
pub const fn is_bare_metal() -> bool {
    ON_BARE_METAL
}

/// Invalidate one TLB entry.
///
/// A no-op off bare metal. This is not a test convenience — it is required for
/// the address-space logic to be unit-testable at all, because `INVLPG` is a
/// privileged instruction and would fault under the host test runner. Every
/// caller is still correct on bare metal, where the real instruction runs.
pub fn invlpg(va: u64) {
    if !ON_BARE_METAL {
        return;
    }
    // SAFETY: `INVLPG` only affects the TLB entry for `va`; it cannot fault
    // even when the address is unmapped, which is exactly when it is needed.
    unsafe {
        asm!("invlpg [{va}]", va = in(reg) va, options(nomem, nostack, preserves_flags));
    }
}

/// Flush the whole non-global TLB.
///
/// A bare `mov cr3, cr3` reloads the PML4 base, discarding every non-global
/// entry. With no `PCID` in play this is the only way to unmap a global kernel
/// page, which is why [`AddressSpace::map_range`] uses it and
/// [`AddressSpace::unmap_page`] prefers `invlpg`.
pub fn flush() {
    if !ON_BARE_METAL {
        return;
    }
    // SAFETY: reloading the current CR3 is always valid.
    unsafe {
        asm!(
            "mov rax, cr3; mov cr3, rax",
            options(nomem, nostack, preserves_flags)
        );
    }
}

/// Physical addresses below this are reserved by the CPU and must never be
/// mapped or allocated: the real-mode IVT, BIOS data area, and the legacy
/// 0xA0000–0xFFFFF window.
pub const LOW_RESERVED_LIMIT: u64 = 0x10_0000;

/// True when `pa` is in the low reserved region or is zero.
pub const fn is_reserved_physical(pa: u64) -> bool {
    pa < LOW_RESERVED_LIMIT
}

/// Where large-page-friendly kernel mappings go.
///
/// The higher half starts at the canonical boundary, so kernel addresses have
/// bit 47 set and never collide with the low half userspace will occupy.
pub const KERNEL_VIRTUAL_BASE: u64 = 0xFFFF_8000_0000_0000;

/// The first 4 KiB, mapped at [`KERNEL_VIRTUAL_BASE`] and also identity-mapped,
/// so a fault in the trampoline can always be reported.
pub const KERNEL_VIRTUAL_END: u64 = KERNEL_VIRTUAL_BASE + 0x1000_0000;

/// Identity-map base, used during boot before the higher-half switch and kept
/// afterwards for the framebuffer and for MMIO.
pub const IDENTITY_BASE: u64 = 0;

/// True when `va` lies in the kernel's higher half.
pub const fn is_kernel_canonical(va: u64) -> bool {
    va >= KERNEL_VIRTUAL_BASE
}

/// A page-table entry as seen by tests and debug output.
pub fn describe(entry: PageTableEntry) -> PageFlags {
    PageFlags {
        readable: entry.is_present(),
        writable: entry.0 & entry_flags::WRITABLE != 0,
        // `NX` set means "forbidden", so executable means the bit is clear.
        executable: entry.is_leaf() && entry.0 & entry_flags::NX == 0,
        user: entry.0 & entry_flags::USER != 0,
        user_writable: entry.0 & (entry_flags::USER | entry_flags::WRITABLE)
            == (entry_flags::USER | entry_flags::WRITABLE),
        uncacheable: entry.0 & entry_flags::NO_CACHE != 0,
    }
}

#[cfg(test)]
extern crate std;

#[cfg(test)]
mod tests {
    use super::*;
    use std::string::String;
    use std::vec::Vec;

    #[test]
    fn split_round_trips() {
        // Only canonical addresses round-trip: the decomposition keeps the
        // low 48 bits, so it cannot distinguish `0x0000_8000_0000_0000` from
        // `0xFFFF_8000_0000_0000` — and the former faults on the CPU anyway.
        for va in [
            0u64,
            0x1000,
            0x0000_7FFF_FFFF_F000,
            KERNEL_VIRTUAL_BASE,
            KERNEL_VIRTUAL_BASE + 0x123_4567,
            0xFFFF_FFFF_FFFF_F000,
        ] {
            assert!(is_canonical(va), "test input must be canonical");
            let s = Split::of(va);
            assert_eq!(s.base_of() + s.offset, va, "round-trip failed for {va:#x}");
        }
    }

    #[test]
    fn non_canonical_addresses_are_recognised() {
        // Bit 47 set with a non-sign-extended high half: the CPU raises a GP
        // fault for these, so the kernel must never map them.
        assert!(!is_canonical(0x0000_8000_0000_0000));
        assert!(!is_canonical(0x0000_FFFF_FFFF_F000));
        assert!(is_canonical(0xFFFF_8000_0000_0000));
        assert!(is_canonical(0x0000_0000_0000_0000));
    }

    #[test]
    fn split_indices_are_in_range() {
        for i in 0..4096u64 {
            let s = Split::of(i * 0x1234_5678_9ABC);
            for idx in s.path() {
                assert!(idx < ENTRIES_PER_TABLE);
            }
            assert!(s.offset < PAGE_SIZE);
        }
    }

    #[test]
    fn split_of_all_ones_uses_canonical_low_address() {
        // 0xFFFF_FFFF_FFFF_FFFF is the sign-extended form of -1. The index
        // bits come from bits 47..=39, not from the sign extension, so the
        // PML4 index is 511 and the base is the topmost canonical page.
        let s = Split::of(0xFFFF_FFFF_FFFF_FFFF);
        assert_eq!(s.pml4, 0x1FF);
        assert_eq!(s.base_of(), 0xFFFF_FFFF_FFFF_F000);
        assert_eq!(s.offset, 0xFFF);
    }

    #[test]
    fn level_index_selects_the_right_bits() {
        let va = 0x1234_5678_9ABC_DEF0;
        // Computed from the bit positions, not guessed: PML4 takes bits 39..47,
        // PDPT 30..38, PD 21..29, PT 12..20.
        assert_eq!(PageLevel::PageMapLevel4.index(va), 172);
        assert_eq!(PageLevel::PageTableDirectoryPointer.index(va), 482);
        assert_eq!(PageLevel::PageDirectory.index(va), 213);
        assert_eq!(PageLevel::PageTable.index(va), 461);
    }

    #[test]
    fn level_parent_child_are_inverses() {
        for level in LEVELS {
            if let Some(p) = level.parent() {
                assert_eq!(p.child(), Some(level));
            }
            if let Some(c) = level.child() {
                assert_eq!(c.parent(), Some(level));
            }
        }
        assert_eq!(PageLevel::PageMapLevel4.parent(), None);
        assert_eq!(PageLevel::PageTable.child(), None);
    }

    #[test]
    fn bytes_covered_doubles_per_level() {
        assert_eq!(bytes_covered(PageLevel::PageTable as u8), 4096);
        assert_eq!(
            bytes_covered(PageLevel::PageDirectory as u8),
            2 * 1024 * 1024
        );
        assert_eq!(
            bytes_covered(PageLevel::PageTableDirectoryPointer as u8),
            1024 * 1024 * 1024
        );
        assert_eq!(
            bytes_covered(PageLevel::PageMapLevel4 as u8),
            512 * 1024 * 1024 * 1024
        );
    }

    #[test]
    fn wx_request_is_never_honoured() {
        // A caller asking for writable+executable gets non-executable. This is
        // the whole point of encoding W^X once instead of trusting call sites.
        let bad = PageFlags {
            writable: true,
            executable: true,
            ..PageFlags::kernel_rw()
        };
        assert!(bad.is_wx_violation());
        let bits = encode_flags(bad);
        // `NX` set means "forbidden", so a W^X violation must end up NX.
        assert_ne!(bits & entry_flags::NX, 0, "must be NX");
        assert_ne!(bits & entry_flags::WRITABLE, 0, "must stay writable");
        assert!(!encoded_allows_execute(bits));
    }

    #[test]
    fn code_pages_are_executable_but_not_writable() {
        let bits = encode_flags(PageFlags::kernel_rx());
        // Executable means NX *clear*.
        assert_eq!(bits & entry_flags::NX, 0);
        assert_eq!(bits & entry_flags::WRITABLE, 0);
        assert_ne!(bits & entry_flags::PRESENT, 0);
        assert_eq!(bits & entry_flags::USER, 0);
        assert!(encoded_allows_execute(bits));
    }

    #[test]
    fn data_pages_are_writable_but_not_executable() {
        let bits = encode_flags(PageFlags::kernel_rw());
        // Non-executable means NX *set*.
        assert_ne!(bits & entry_flags::NX, 0);
        assert_ne!(bits & entry_flags::WRITABLE, 0);
        assert!(!encoded_allows_execute(bits));
    }

    #[test]
    fn user_pages_set_the_user_bit() {
        assert_ne!(encode_flags(PageFlags::user_rw()) & entry_flags::USER, 0);
        assert_ne!(encode_flags(PageFlags::user_rx()) & entry_flags::USER, 0);
        assert_eq!(encode_flags(PageFlags::kernel_rw()) & entry_flags::USER, 0);
    }

    #[test]
    fn mmio_disables_caching() {
        let bits = encode_flags(PageFlags::mmio());
        assert_ne!(bits & entry_flags::NO_CACHE, 0);
        assert_ne!(bits & entry_flags::WRITE_THROUGH, 0);
    }

    #[test]
    fn entry_construction_preserves_the_physical_address() {
        let e = PageTableEntry::new_4kib(0xDEAD_B000 | 0xFFF, PageFlags::kernel_rw());
        assert_eq!(e.physical(), 0xDEAD_B000);
        assert!(e.is_present());
        assert!(e.is_leaf());
        assert!(!e.is_large());
    }

    #[test]
    fn large_pages_are_marked_and_are_not_leaves() {
        let e = PageTableEntry::new_large(2 * 1024 * 1024, PageFlags::kernel_rw());
        assert!(e.is_large());
        // `is_leaf` means "not a pointer to another table", and a large page is
        // a leaf; the PSE bit is what distinguishes it from a 4 KiB leaf.
        assert!(e.is_leaf());
        assert_eq!(e.physical(), 2 * 1024 * 1024);
    }

    #[test]
    fn empty_entry_is_absent() {
        let e = PageTableEntry::empty();
        assert!(!e.is_present());
        assert_eq!(e.physical(), 0);
    }

    #[test]
    fn describe_round_trips_through_encode() {
        for flags in [
            PageFlags::kernel_rw(),
            PageFlags::kernel_rx(),
            PageFlags::kernel_ro(),
            PageFlags::user_rw(),
            PageFlags::user_rx(),
            PageFlags::mmio(),
        ] {
            let entry = PageTableEntry::new_4kib(0x1000, flags);
            let back = describe(entry);
            assert_eq!(back.readable, flags.readable);
            assert_eq!(back.writable, flags.writable);
            assert_eq!(back.user, flags.user);
            assert_eq!(back.uncacheable, flags.uncacheable);
        }
    }

    #[test]
    fn reserved_physical_range_is_excluded() {
        assert!(is_reserved_physical(0));
        assert!(is_reserved_physical(LOW_RESERVED_LIMIT - 1));
        assert!(!is_reserved_physical(LOW_RESERVED_LIMIT));
    }

    #[test]
    fn kernel_half_is_recognised() {
        assert!(is_kernel_canonical(KERNEL_VIRTUAL_BASE));
        assert!(!is_kernel_canonical(0));
        assert!(!is_kernel_canonical(0x0000_7FFF_FFFF_FFFF));
    }

    #[test]
    fn kernel_window_size_is_one_gibibyte() {
        assert_eq!(KERNEL_VIRTUAL_END - KERNEL_VIRTUAL_BASE, 0x1000_0000);
    }

    #[test]
    fn table_geometry_is_consistent() {
        assert_eq!(TABLE_BYTES, 4096);
        assert_eq!(ENTRIES_PER_TABLE * size_of::<PageTableEntry>(), 4096);
    }

    #[test]
    fn address_mask_covers_exactly_the_page_frame() {
        let e = PageTableEntry::new_4kib(0x1234_5678_9000, PageFlags::kernel_rw());
        assert_eq!(e.physical(), 0x1234_5678_9000);
        // Flags must never leak into the address, and a leaf must not carry
        // the table-pointer marker.
        let raw = e.0;
        assert_eq!(
            raw & !entry_flags::ADDRESS_MASK,
            encode_flags(PageFlags::kernel_rw())
        );
        assert_eq!(
            raw & entry_flags::AVAILABLE,
            0,
            "a leaf is not a table pointer"
        );
    }

    #[test]
    fn error_messages_cover_every_failure() {
        // Not a paging test per se, but it pins the contract the mapper
        // relies on: each rejection has a distinct, non-overlapping cause.
        let reasons: Vec<MapError> = [
            MapError::MisalignedVirtual,
            MapError::MisalignedPhysical,
            MapError::AddressOverflow,
            MapError::AlreadyMapped,
            MapError::NotMapped,
            MapError::OutOfMemory,
        ]
        .into_iter()
        .collect();
        let mut text: Vec<String> = reasons
            .iter()
            .map(|e| {
                use core::fmt::Write as _;
                let mut s = String::new();
                let _ = write!(s, "{e}");
                s
            })
            .collect();
        let before = text.len();
        text.sort();
        text.dedup();
        assert_eq!(before, text.len());
    }

    #[test]
    fn table_pointers_are_distinguished_from_leaves() {
        // The `AVAILABLE` marker is what lets the walker tell "points at a
        // lower table" from "maps memory" without relying on the ambiguous
        // `PS` bit.
        let pointer =
            PageTableEntry(entry_flags::PRESENT | entry_flags::WRITABLE | entry_flags::AVAILABLE);
        assert!(pointer.is_table_pointer());
        assert!(!pointer.is_leaf());

        let leaf = PageTableEntry::new_4kib(0x1000, PageFlags::kernel_rw());
        assert!(leaf.is_leaf());
        assert!(!leaf.is_table_pointer());
    }
}
