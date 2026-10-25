//! The global descriptor table and the task state segment.
//!
//! # Why the kernel owns its GDT
//!
//! OVMF hands the loader a working GDT, and the loader runs on it happily.
//! The kernel cannot: the `syscall`/`sysret` pair derives its segment
//! selectors from `IA32_STAR` with *fixed* offsets — `CS` is
//! `IA32_STAR[47:32]` and `SS` is that plus 8 on entry, while on return
//! `CS` is `IA32_STAR[63:48] + 16` and `SS` is that plus 8. Those arithmetic
//! relationships constrain the GDT's *order*, not just its contents, and the
//! firmware's table has no reason to satisfy them. So the kernel installs its
//! own.
//!
//! # The layout
//!
//! ```text
//! index  selector  descriptor
//!   0       0x00    null
//!   1       0x08    kernel code (ring 0, 64-bit, execute/read)
//!   2       0x10    kernel data (ring 0, read/write)
//!   3       0x18    user data   (ring 3, read/write)
//!   4       0x20    user code   (ring 3, 64-bit)
//!   5       0x28    TSS, low half
//!   6       0x30    TSS, high half
//! ```
//!
//! # Why the TSS takes two entries
//!
//! A GDT entry is **8 bytes**. The TSS is a 16-byte system descriptor, so it
//! occupies two consecutive entries, and `TSS_SELECTOR` names the first of
//! them. Every code/data descriptor occupies exactly one.
//!
//! The table is therefore seven entries long, and — this is the part that
//! bites — every descriptor must be stored at the entry its selector names.
//! A selector is an *index*, and the CPU computes the entry's byte offset as
//! `index * 8`; storing the table at 16-byte strides would shift every
//! descriptor one entry along and make the selector name the wrong
//! descriptor entirely.
//!
//! # Why the user descriptors are in that order
//!
//! The order is not a style choice; it is forced by the `syscall`/`sysret`
//! arithmetic, and getting it wrong produces a `#GP` on the first syscall
//! with no useful diagnostic.
//!
//! * `syscall` loads `CS` from `IA32_STAR[47:32]` and `SS` from that value
//!   plus 8. Those must be the *kernel* descriptors, so `IA32_STAR[47:32]`
//!   is `0x08` and `SS` becomes `0x10`.
//! * `sysret` loads `CS` from `IA32_STAR[63:48] + 16` and `SS` from
//!   `IA32_STAR[63:48] + 8`, with RPL forced to 3. Those must be the *user*
//!   descriptors, so the user data descriptor must sit 8 bytes below the
//!   user code descriptor: `0x18` and `0x20`. That makes
//!   `IA32_STAR[63:48] = 0x10`.
//!
//! With that order, `STAR_VALUE` is `(0x10 << 48) | (0x08 << 32)`.
//!
//! # The descriptor bit layout
//!
//! A descriptor's *low* 64 bits are laid out as follows, and every field this
//! module reads or writes is listed:
//!
//! ```text
//!  0-15  limit 15..0
//! 16-31  base 15..0
//! 32-39  base 23..16
//! 40-43  type
//!    44  S   (0 for a system segment, 1 for code/data)
//! 45-46  DPL
//!    47  P   (present)
//! 48-51  limit 19..16
//!    52  AVL
//!    53  L   (long mode)
//!    54  D/B
//!    55  G   (granularity)
//! 56-63  base 31..24
//! ```
//!
//! The *high* 64 bits of a 16-byte system descriptor carry the base's high
//! bits: `0-31` hold base bits 32..63, and the rest is reserved.
//!
//! # Why the descriptor bytes are spelled out
//!
//! Getting a single bit wrong produces a `#GP` at a moment with no useful
//! diagnostic — the machine dies on the first segment reload. The byte
//! patterns here are the ones Linux uses, which are the ones every emulator
//! and every CPU has been tested against.

use core::arch::asm;

use crate::regs;

/// The selector of the kernel's code segment.
pub const KERNEL_CODE_SELECTOR: u16 = 0x08;

/// The selector of the kernel's data segment.
pub const KERNEL_DATA_SELECTOR: u16 = 0x10;

/// The selector of the ring-3 data segment.
pub const USER_DATA_SELECTOR: u16 = 0x18;

/// The selector of the ring-3 code segment.
pub const USER_CODE_SELECTOR: u16 = 0x20;

/// The selector of the task state segment.
pub const TSS_SELECTOR: u16 = 0x28;

/// The number of GDT entries, including the null descriptor.
///
/// Seven: null, kernel code, kernel data, user data, user code, and the two
/// halves of the TSS.
///
/// # Why seven and not six
///
/// A GDT entry is **8 bytes**. Four of them describe the code and data
/// segments, and the TSS is a **16-byte** system descriptor, so it occupies
/// *two* consecutive entries — the low half and the high half. Six
/// *descriptors* therefore span seven *entries*.
///
/// # Why this matters
///
/// The distinction is not pedantry: it is the difference between a table the
/// CPU can read and one it cannot. A selector names an **entry index**,
/// and the CPU computes the entry's byte offset as `index * 8`. Writing a
/// descriptor at a 16-byte stride therefore puts every descriptor one entry
/// too far along — the kernel code lands in entry 1 (which happens to be
/// right, because the null descriptor occupies entry 0 and the code's *low*
/// half is at byte 8), while the kernel data lands in entry 3, the user data
/// in entry 5, and the TSS in entries 9 and 10.
///
/// The visible symptom is a `#GP` on the very first `LTR`, because entry 5 —
/// the one `TSS_SELECTOR` (0x28) names — holds the *user data* descriptor,
/// and a data segment is not a task state segment.
pub const GDT_ENTRIES: usize = 7;

/// Bytes per GDT entry.
///
/// Eight, not sixteen: every GDT entry is eight bytes wide, and the TSS is
/// the only 16-byte object in the table because it is *two* entries.
pub const GDT_ENTRY_BYTES: usize = 8;

/// Total bytes of the GDT.
pub const GDT_BYTES: usize = GDT_ENTRIES * GDT_ENTRY_BYTES;

// --- descriptor encodings -------------------------------------------------
//
// The values below are the canonical ones — the ones Linux uses, and the ones
// the `x86_64` crate's own unit tests assert. A change here would be a
// silent change of meaning.

/// A 64-bit ring-0 code segment: execute/read, present, long mode.
pub const KERNEL_CODE_DESCRIPTOR: u64 = 0x00AF_9B00_0000_FFFF;

/// A ring-0 data segment: read/write, present, 32-bit default size.
pub const KERNEL_DATA_DESCRIPTOR: u64 = 0x00CF_9300_0000_FFFF;

/// A 64-bit ring-3 code segment: execute/read, present, long mode.
pub const USER_CODE_DESCRIPTOR: u64 = 0x00AF_FB00_0000_FFFF;

/// A ring-3 data segment: read/write, present, 32-bit default size.
pub const USER_DATA_DESCRIPTOR: u64 = 0x00CF_F300_0000_FFFF;

/// Bit positions within a descriptor's low 64 bits.
///
/// These are spelled out rather than inlined into the constants above so
/// that the tests can assert *why* each constant is what it is.
pub mod bit {
    /// The type field: the segment's kind and its access permissions.
    pub const TYPE: u32 = 40;
    /// The `S` bit: clear for a system segment, set for code or data.
    pub const SYSTEM: u32 = 44;
    /// The descriptor privilege level: the ring the segment may be used at.
    pub const DPL: u32 = 45;
    /// The present bit.
    pub const PRESENT: u32 = 47;
    /// The limit's high nibble.
    pub const LIMIT_HIGH: u32 = 48;
    /// The long-mode bit. Set only on 64-bit code segments.
    pub const LONG_MODE: u32 = 53;
    /// The default-operand-size bit. Must be clear when `LONG_MODE` is set.
    pub const DEFAULT_SIZE: u32 = 54;
    /// The granularity bit.
    pub const GRANULARITY: u32 = 55;
    /// The base's high byte.
    pub const BASE_HIGH: u32 = 56;
}

/// The type of an available 64-bit task state segment.
pub const TSS_TYPE: u64 = 0b1001;

/// The size of the 64-bit task state segment, in bytes.
///
/// The 64-bit TSS is laid out as:
///
/// ```text
///   0-  3  reserved
///   4- 11  RSP0      the ring-0 stack for an interrupt from ring 3
///  12- 19  RSP1
///  20- 27  RSP2
///  28- 35  reserved
///  36- 91  IST1..IST7
///  92- 99  reserved
/// 100-101  reserved
/// 102-103  reserved
/// 104-105  I/O map base
/// ```
///
/// so the I/O map base field is at offset 104 and the segment must be at
/// least 106 bytes long. It is made 112 here so that the field is comfortably
/// inside the segment and the limit is 8-byte aligned.
pub const TSS_BYTES: usize = 112;

/// The byte offset of the TSS's `RSP0` field.
pub const TSS_RSP0_OFFSET: usize = 4;

/// The byte offset of the TSS's I/O map base address.
pub const TSS_IO_MAP_OFFSET: usize = 104;

/// The I/O map base offset for a TSS with no permission bitmap.
///
/// 0xDFFF is the architectural maximum: the bitmap may occupy at most the
/// 0xDFFF bytes preceding the end of the segment, and a TSS that declares no
/// bitmap uses this value to say so.
///
/// The value must be *greater than the segment limit*, or the CPU would look
/// for a bitmap that is not there. With a 112-byte TSS the limit is 111, and
/// 0xDFFF is far above it, so the CPU concludes there is no bitmap — which
/// denies every ring-3 port access, which is what the kernel wants.
pub const TSS_IO_MAP_BASE: u64 = 0xDFFF;

/// Build the 16-byte system descriptor that names the TSS at `base`.
///
/// # Safety
///
/// `base` must be the address of a TSS that stays valid for the life of the
/// CPU, and the TSS must be at least [`TSS_BYTES`] long.
pub fn tss_descriptor(base: u64) -> [u64; 2] {
    // The limit is inclusive, so a 112-byte TSS has limit 111.
    let limit = (TSS_BYTES - 1) as u64;
    // The low half carries the limit, the low 24 bits of the base, the type,
    // the present bit, the limit's high nibble and the base's high byte.
    //
    // `base` bits 0..=23 go at bits 16..=39, and base bits 24..=31 go at
    // bits 56..=63 — which is the architectural split of the base field, not
    // a contiguous one.
    let low = (limit & 0xFFFF)
        | ((base & 0x00FF_FFFF) << 16)
        | (TSS_TYPE << bit::TYPE)
        | (1u64 << bit::PRESENT)
        | (((limit >> 16) & 0xF) << bit::LIMIT_HIGH)
        | (((base >> 24) & 0xFF) << bit::BASE_HIGH);
    // The high half carries base bits 32..=63 in bits 0..=31. The rest of the
    // 128-bit descriptor is reserved and must be zero.
    let high = (base >> 32) & 0xFFFF_FFFF;
    [low, high]
}

/// The kernel's GDT.
///
/// A `static mut` array rather than a `Sync` wrapper because the table is
/// written exactly once, during boot, before any other CPU-visible thread
/// exists. Every access is therefore behind a `&mut` obtained through
/// [`core::ptr::addr_of_mut!`], which is the only sound way to reach a
/// `static mut` and the reason this does not need a lock.
///
/// The array is `[u64; GDT_ENTRIES]`, not `[u64; GDT_ENTRIES * 2]`: a GDT
/// entry is **8 bytes**, and the table's entries are indexed by the selector
/// the CPU uses. Storing the table at 16-byte strides would double every
/// descriptor's distance from the base and shift every selector's meaning.
static mut GDT: [u64; GDT_ENTRIES] = [0; GDT_ENTRIES];

/// The kernel's task state segment.
///
/// A byte array rather than a `u64` array, because the 64-bit TSS's fields
/// are not 8-byte aligned: `RSP0` starts at offset 4 and the I/O map base at
/// offset 104. Indexing a `u64` array by "the field at offset N" would be
/// wrong for every field except by accident, and the mistake would be a
/// silent one — the CPU would read a different value than the kernel wrote.
static mut TSS: [u8; TSS_BYTES] = [0; TSS_BYTES];

/// Write a little-endian `u64` into `buffer` at `offset`.
///
/// # Safety
///
/// `offset + 8` must be within `buffer`.
unsafe fn write_u64(buffer: &mut [u8], offset: usize, value: u64) {
    // SAFETY: the caller guarantees the eight bytes at `offset` are inside
    // `buffer`, and `write_unaligned` does not require alignment.
    unsafe {
        core::ptr::write_unaligned(buffer.as_mut_ptr().add(offset) as *mut u64, value.to_le());
    }
}

/// Read a little-endian `u64` from `buffer` at `offset`.
///
/// # Safety
///
/// `offset + 8` must be within `buffer`.
unsafe fn read_u64(buffer: &[u8], offset: usize) -> u64 {
    // SAFETY: the caller guarantees the eight bytes at `offset` are inside
    // `buffer`, and `read_unaligned` does not require alignment.
    unsafe {
        u64::from_le(core::ptr::read_unaligned(
            buffer.as_ptr().add(offset) as *const u64
        ))
    }
}

/// Install the kernel's GDT and TSS.
///
/// # What it does
///
/// 1. Writes the six descriptors into the static GDT.
/// 2. Loads the GDT with `LGDT`.
/// 3. Reloads `CS` and the data segment registers so the CPU actually uses
///    the new table.
/// 4. Loads the TSS descriptor with `LTR`.
///
/// # Why `LGDT` is not optional
///
/// The step that makes the whole table take effect is the `LGDT` itself.
/// Without it the CPU keeps using the *firmware's* descriptor table, and
/// every selector this module defines — `0x08`, `0x10`, `0x18`, `0x20`,
/// `0x28` — names a descriptor in OVMF's table, or nothing at all. The
/// `CS` reload then loads a descriptor that is not the kernel code segment
/// and the machine faults with a `#GP` that names no file and no line.
///
/// So the `LGDT` comes *before* the segment reloads, and the table it points
/// at is the one this function has just finished writing.
///
/// # Why `CS` must be reloaded
///
/// `LGDT` does not change the descriptor the CPU has cached for `CS`; it
/// only changes where a *future* segment load will look. Until `CS` is
/// reloaded, the kernel keeps running on the firmware's descriptor — and the
/// first far return (an `iretq` back from an interrupt, say) would reload
/// `CS` from the *new* table, where the firmware's selector may name
/// something else entirely. The result is a `#GP` with no obvious cause.
///
/// The architectural way to force the reload is a far return, which pops
/// the target, the selector and the flags in one step.
///
/// # Why the caller must have interrupts disabled
///
/// Between the `LGDT` and the `CS` reload there is a window in which the CPU
/// holds the *old* `CS` but would reload it from the *new* table. An
/// interrupt taken in that window returns through a selector the new table
/// may not contain, which is a `#GP` on a path the developer cannot see.
/// Disabling interrupts closes the window.
///
/// # Safety
///
/// Must be called once, with interrupts disabled, before any segment selector
/// other than the firmware's is used.
pub unsafe fn install() {
    // SAFETY: this runs once during boot, before any other CPU-visible
    // thread exists, so nothing else observes the tables being written.
    unsafe {
        let gdt = &mut *core::ptr::addr_of_mut!(GDT);
        // Entry 0 is the null descriptor.
        gdt[0] = 0;
        // Entry 1 (selector 0x08): the kernel's 64-bit code segment.
        gdt[1] = KERNEL_CODE_DESCRIPTOR;
        // Entry 2 (selector 0x10): the kernel's data segment.
        gdt[2] = KERNEL_DATA_DESCRIPTOR;
        // Entry 3 (selector 0x18): the ring-3 data segment.
        gdt[3] = USER_DATA_DESCRIPTOR;
        // Entry 4 (selector 0x20): the ring-3 code segment.
        gdt[4] = USER_CODE_DESCRIPTOR;
        // Entry 5 (selector 0x28): the TSS's low half.
        // Entry 6 (selector 0x30): the TSS's high half.
        //
        // The TSS is the one 16-byte object in the table, so it spans two
        // entries — and `TSS_SELECTOR` names the first of them.

        // The TSS. Only the fields that matter are set; the rest stay zero,
        // which the CPU reads as "base 0".
        let tss = &mut *core::ptr::addr_of_mut!(TSS);
        // The I/O map base lives at TSS offset 104.
        write_u64(tss, TSS_IO_MAP_OFFSET, TSS_IO_MAP_BASE);

        let tss_base = tss.as_ptr() as u64;
        let desc = tss_descriptor(tss_base);
        let gdt = &mut *core::ptr::addr_of_mut!(GDT);
        gdt[5] = desc[0];
        gdt[6] = desc[1];

        // Load the table. The descriptor pointer's *base* is the address of
        // the first byte of the table and its *limit* is the offset of the
        // last byte, so the limit is `GDT_BYTES - 1`.
        //
        // The `limit` is what makes the CPU able to read the TSS descriptor
        // at all: without it the CPU would consider the table to end before
        // the TSS and refuse the `LTR` that follows.
        let gdtr = regs::DescriptorTablePointer {
            limit: (GDT_BYTES - 1) as u16,
            base: gdt.as_ptr() as u64,
        };
        regs::lgdt(gdtr);

        // Reload the segments now that the new table is in force. `CS` first:
        // the far return that reloads it is the architecturally defined way
        // to make the CPU re-read the descriptor.
        regs::reload_code_segment(KERNEL_CODE_SELECTOR);
        regs::reload_data_segments(KERNEL_DATA_SELECTOR);

        // The TSS descriptor is in the GDT and the GDT is loaded, so `LTR`
        // can now find it by selector.
        regs::load_tr(TSS_SELECTOR);
    }
}

/// Set the ring-0 stack the CPU switches to when an interrupt is taken from
/// ring 3.
///
/// # Safety
///
/// `stack_top` must be the top of a mapped, 16-byte-aligned kernel stack
/// that stays valid for as long as ring-3 code can run.
pub unsafe fn set_ring0_stack(stack_top: u64) {
    // SAFETY: this runs once per task switch, before the task is activated.
    unsafe {
        let tss = &mut *core::ptr::addr_of_mut!(TSS);
        // RSP0 is at TSS offset 4: a 64-bit value, little-endian.
        write_u64(tss, TSS_RSP0_OFFSET, stack_top);
    }
}

/// Read the ring-0 stack the CPU would switch to.
pub fn ring0_stack() -> u64 {
    // SAFETY: the TSS is a `static mut` written only under the boot-time
    // single-thread assumption; this read observes the same word the setter
    // wrote.
    unsafe {
        let tss = &*core::ptr::addr_of!(TSS);
        read_u64(tss, TSS_RSP0_OFFSET)
    }
}

/// True when the task register names this kernel's TSS.
///
/// Reading the task register is the only way to confirm that `LTR` took
/// effect, and it is worth confirming: a failed `LTR` leaves the CPU with
/// whatever TSS the firmware had, and the first interrupt taken from ring 3
/// would then switch to a stack the kernel does not own.
pub fn tss_is_loaded() -> bool {
    if !regs::ON_BARE_METAL {
        return false;
    }
    let tr: u16;
    // SAFETY: `str` reads the task register, which is always readable in
    // ring 0 and has no side effects.
    unsafe {
        asm!("str {tr:x}", tr = out(reg) tr, options(nomem, nostack, preserves_flags));
    }
    tr == TSS_SELECTOR
}

#[cfg(test)]
extern crate std;

#[cfg(test)]
mod tests {
    use super::*;

    /// The type field of a code segment: execute/read, accessed.
    const CODE_TYPE: u64 = 0b1011;
    /// The type field of a data segment: read/write, accessed.
    const DATA_TYPE: u64 = 0b0011;

    #[test]
    fn the_selectors_are_the_documented_ones() {
        assert_eq!(KERNEL_CODE_SELECTOR, 0x08);
        assert_eq!(KERNEL_DATA_SELECTOR, 0x10);
        assert_eq!(USER_DATA_SELECTOR, 0x18);
        assert_eq!(USER_CODE_SELECTOR, 0x20);
        assert_eq!(TSS_SELECTOR, 0x28);
    }

    #[test]
    fn the_syscall_offsets_land_on_the_right_descriptors() {
        // This is the constraint that dictates the whole GDT order.
        //
        // `IA32_STAR[47:32]` is the kernel `CS` on `syscall` entry, and `SS`
        // is that value plus 8.
        let syscall_cs = KERNEL_CODE_SELECTOR;
        let syscall_ss = syscall_cs + 8;
        assert_eq!(syscall_cs, KERNEL_CODE_SELECTOR);
        assert_eq!(syscall_ss, KERNEL_DATA_SELECTOR);

        // `IA32_STAR[63:48]` is the *base* for `sysret`: `CS` is that plus
        // 16 and `SS` is that plus 8, both with RPL forced to 3.
        let sysret_base = USER_CODE_SELECTOR - 16;
        assert_eq!(sysret_base + 16, USER_CODE_SELECTOR);
        assert_eq!(sysret_base + 8, USER_DATA_SELECTOR);
    }

    #[test]
    fn the_descriptors_are_the_canonical_ones() {
        // These are Linux's encodings, which every CPU and emulator has been
        // tested against. A change here would be a silent change of meaning.
        assert_eq!(KERNEL_CODE_DESCRIPTOR, 0x00AF_9B00_0000_FFFF);
        assert_eq!(KERNEL_DATA_DESCRIPTOR, 0x00CF_9300_0000_FFFF);
        assert_eq!(USER_CODE_DESCRIPTOR, 0x00AF_FB00_0000_FFFF);
        assert_eq!(USER_DATA_DESCRIPTOR, 0x00CF_F300_0000_FFFF);
    }

    #[test]
    fn the_descriptor_fields_are_what_they_claim() {
        for (name, desc, dpl, ty) in [
            ("kernel code", KERNEL_CODE_DESCRIPTOR, 0u64, CODE_TYPE),
            ("kernel data", KERNEL_DATA_DESCRIPTOR, 0u64, DATA_TYPE),
            ("user code", USER_CODE_DESCRIPTOR, 0b11u64, CODE_TYPE),
            ("user data", USER_DATA_DESCRIPTOR, 0b11u64, DATA_TYPE),
        ] {
            // Present.
            assert_ne!(desc & (1u64 << bit::PRESENT), 0, "{name} must be present");
            // `S` is set for a code or data segment.
            assert_eq!(
                (desc >> bit::SYSTEM) & 1,
                1,
                "{name} must be a code/data segment"
            );
            // The DPL is the ring the segment may be used at.
            assert_eq!((desc >> bit::DPL) & 0b11, dpl, "{name} has the wrong DPL");
            // The type field.
            assert_eq!((desc >> bit::TYPE) & 0xF, ty, "{name} has the wrong type");
            // The limit is the full 4 GiB.
            assert_eq!(desc & 0xFFFF, 0xFFFF);
            assert_eq!((desc >> bit::LIMIT_HIGH) & 0xF, 0xF);
            // Granularity is set.
            assert_ne!(desc & (1u64 << bit::GRANULARITY), 0);
        }
    }

    #[test]
    fn the_code_descriptors_are_long_mode_and_the_data_ones_are_not() {
        // The `L` bit must be set on a 64-bit code segment, and the `D/B` bit
        // must be clear when it is: setting both is a `#GP` at load time.
        for (name, desc) in [
            ("kernel code", KERNEL_CODE_DESCRIPTOR),
            ("user code", USER_CODE_DESCRIPTOR),
        ] {
            assert_ne!(
                desc & (1u64 << bit::LONG_MODE),
                0,
                "{name} must be long mode"
            );
            assert_eq!(
                desc & (1u64 << bit::DEFAULT_SIZE),
                0,
                "{name} must not also set D/B"
            );
        }
        // A data segment must have `L` clear, and `D/B` set so that it has
        // 32-bit default operand size.
        for (name, desc) in [
            ("kernel data", KERNEL_DATA_DESCRIPTOR),
            ("user data", USER_DATA_DESCRIPTOR),
        ] {
            assert_eq!(
                desc & (1u64 << bit::LONG_MODE),
                0,
                "{name} must not be long mode"
            );
            assert_ne!(desc & (1u64 << bit::DEFAULT_SIZE), 0, "{name} must set D/B");
        }
    }

    #[test]
    fn the_tss_descriptor_names_the_tss() {
        let base = 0xFFFF_8000_0090_0000u64;
        let [low, high] = tss_descriptor(base);
        // Type 0b1001: available 64-bit TSS.
        assert_eq!((low >> bit::TYPE) & 0xF, TSS_TYPE);
        // Present.
        assert_ne!(low & (1u64 << bit::PRESENT), 0);
        // The `S` bit is clear: a TSS is a system segment.
        assert_eq!((low >> bit::SYSTEM) & 1, 0);
        // The limit is inclusive and covers the architectural TSS.
        assert_eq!(low & 0xFFFF, (TSS_BYTES - 1) as u64);
        // The base is split: bits 0..=23 at bits 16..=39, bits 24..=31 at
        // bits 56..=63, and bits 32..=63 in the high half.
        let base_low_24 = (low >> 16) & 0x00FF_FFFF;
        assert_eq!(base_low_24, base & 0x00FF_FFFF);
        assert_eq!((low >> bit::BASE_HIGH) & 0xFF, (base >> 24) & 0xFF);
        assert_eq!(high & 0xFFFF_FFFF, (base >> 32) & 0xFFFF_FFFF);
        // The high half's top 32 bits are reserved and must be zero.
        assert_eq!(high >> 32, 0);
    }

    #[test]
    fn the_tss_descriptor_covers_a_low_base() {
        // A TSS in the low identity-mapped region is the case the loader's
        // own tables use, and the base split must work there too.
        let base = 0x0000_0000_001D_C000u64;
        let [low, high] = tss_descriptor(base);
        assert_eq!((low >> 16) & 0x00FF_FFFF, base & 0x00FF_FFFF);
        assert_eq!((low >> bit::BASE_HIGH) & 0xFF, (base >> 24) & 0xFF);
        assert_eq!(high, 0);
    }

    #[test]
    fn the_io_map_base_declares_no_bitmap() {
        // A TSS with no I/O permission bitmap must set the offset to 0xDFFF.
        // Anything else is read as "there is a bitmap at that offset", which
        // for a small offset lands inside the TSS itself.
        assert_eq!(TSS_IO_MAP_BASE, 0xDFFF);
        // And it must lie beyond the segment limit, or the CPU would look for
        // a bitmap that is not there.
        assert!(TSS_IO_MAP_BASE > (TSS_BYTES - 1) as u64);
    }

    #[test]
    fn the_gdt_is_seven_entries_of_eight_bytes() {
        // A GDT entry is 8 bytes. Four code/data descriptors plus the null
        // descriptor fill entries 0..4, and the TSS fills entries 5 and 6.
        assert_eq!(GDT_ENTRIES, 7);
        assert_eq!(GDT_ENTRY_BYTES, 8);
        assert_eq!(GDT_BYTES, 56);
    }

    #[test]
    fn every_descriptor_lands_in_the_entry_its_selector_names() {
        // A selector names an *entry index*, and the CPU computes the entry's
        // byte offset as `index * 8`. So descriptor `i` must live at entry
        // `i` — which means the table's entries are addressed by the selector
        // directly, not at a 16-byte stride.
        //
        // This is the regression test for the bug that made the kernel fault
        // on its very first `LTR`: the table was stored at 16-byte strides, so
        // the kernel data landed in entry 3, the user data in entry 5, and the
        // TSS in entries 9 and 10. The selector 0x28 that `LTR` used named
        // entry 5, which held the *user data* descriptor — and loading a data
        // segment as a task state segment is a `#GP`.
        let entry_of = |selector: u16| selector as usize / GDT_ENTRY_BYTES;
        assert_eq!(entry_of(KERNEL_CODE_SELECTOR), 1);
        assert_eq!(entry_of(KERNEL_DATA_SELECTOR), 2);
        assert_eq!(entry_of(USER_DATA_SELECTOR), 3);
        assert_eq!(entry_of(USER_CODE_SELECTOR), 4);
        assert_eq!(entry_of(TSS_SELECTOR), 5);
        // The table must hold the TSS's high half too.
        assert_eq!(entry_of(TSS_SELECTOR) + 1, 6);
        const {
            assert!(GDT_ENTRIES >= 7);
        }
    }

    #[test]
    fn the_tss_is_long_enough_for_its_io_map_field() {
        // The I/O map base is at offset 104, so the TSS must be at least 112
        // bytes for the field to be inside it.
        const {
            assert!(TSS_BYTES >= TSS_IO_MAP_OFFSET + 8);
        }
        // A 64-bit TSS must be longer than the 104-byte 32-bit one, or `LTR`
        // raises `#GP` in long mode.
        const {
            assert!(TSS_BYTES > 104);
        }
    }
}
