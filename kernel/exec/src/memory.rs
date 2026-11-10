//! The Exec memory policy: the `MEMF_*` flag set and what it means here.
//!
//! # Where the memory actually lives
//!
//! On AmigaOS, `AllocMem` walked Exec's global memory lists — the service
//! *was* the allocator. VelaOS hands each task its own arena on top of the
//! kernel's page mapper (see `kernel/syscall/src/task_space.rs`), so the
//! allocation itself is per-task and page-granular. What this module owns is
//! the part of the classic contract that is *policy*, not mechanism:
//!
//! * the `MEMF_*` flag values, transcribed from AROS's
//!   `compiler/include/exec/memory.h`, because a program written against the
//!   classic header passes them unchanged;
//! * which flags are honoured, which are advisory, and which describe
//!   hardware this platform does not have;
//! * the `AvailMem` accounting modes.
//!
//! Keeping that policy in one place means the syscall boundary validates
//! flags through it rather than re-deriving the meanings ad hoc.

/// The `MEMF_*` flags, with their AmigaOS/AROS values.
///
/// The values come from AROS's `compiler/include/exec/memory.h`, where each
/// flag is `1L << MEMB_*`. They are transcribed rather than reinvented: a
/// program compiled against the classic header passes the same numbers, and
/// a number that differs is a number that silently means something else.
pub mod memf {
    /// Any memory will do.
    pub const ANY: u32 = 0;
    /// Memory that is shared between tasks — the classic "public" pool.
    pub const PUBLIC: u32 = 1 << 0;
    /// Chip RAM: the classic hardware constraint, meaningless here.
    pub const CHIP: u32 = 1 << 1;
    /// Fast memory, as opposed to Chip RAM: every memory here is "fast".
    pub const FAST: u32 = 1 << 2;
    /// Memory that may be mapped into an address space.
    pub const VIRTUAL: u32 = 1 << 3;
    /// Memory that may hold executable code.
    pub const EXECUTABLE: u32 = 1 << 4;
    /// Memory local to the CPU — on a uniprocessor, all of it.
    pub const LOCAL: u32 = 1 << 8;
    /// Must be reachable by 24-bit DMA. A modern bus has no such limit, so
    /// the flag is accepted and always satisfied.
    pub const TWENTY_FOUR_BIT_DMA: u32 = 1 << 9;
    /// The memory can kick a cache of the right kind — an AmigaOS chipset
    /// notion with no counterpart here, so it is accepted and ignored.
    pub const KICK: u32 = 1 << 10;
    /// The allocation is private to the allocating task.
    pub const PRIVATE: u32 = 1 << 11;
    /// The allocation must live in the low two gigabytes.
    pub const THIRTY_ONE_BIT: u32 = 1 << 12;
    /// Zero-fill the allocation.
    pub const CLEAR: u32 = 1 << 16;
    /// `AvailMem`: report the largest free block, not the total.
    pub const LARGEST: u32 = 1 << 17;
    /// `AvailMem`: search the free list from the high end.
    pub const REVERSE: u32 = 1 << 18;
    /// `AvailMem`: report the total rather than the largest block.
    pub const TOTAL: u32 = 1 << 19;
    /// `AllocMem`: align the address and size to a physical page boundary.
    pub const HWALIGNED: u32 = 1 << 20;
    /// `CreatePool`: protect the pool with a semaphore.
    pub const SEM_PROTECTED: u32 = 1 << 20;
    /// Do not expunge to satisfy the request.
    pub const NO_EXPUNGE: u32 = 1 << 31;

    /// The flags that describe the *physical* properties of memory.
    ///
    /// AROS calls this `MEMF_PHYSICAL_MASK` and notes that it is an internal
    /// definition that may change; it is transcribed because the split
    /// between "which memory" and "how the memory may be used" is the
    /// distinction the flag set is built around.
    pub const PHYSICAL_MASK: u32 =
        PUBLIC | CHIP | FAST | LOCAL | TWENTY_FOUR_BIT_DMA | KICK | THIRTY_ONE_BIT;
}

/// The error codes this module reports.
///
/// Re-declared here so this crate keeps its single dependency direction
/// (exec does not depend on the syscall crate, which depends on exec).
pub mod errors {
    /// The value was invalid for the call.
    pub const INVALID_VALUE: i32 = -21;
}

/// The `AvailMem` question a flag word asks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Query {
    /// How many bytes are free in total?
    Total,
    /// How many bytes are in the largest free block?
    Largest,
}

/// What an `AvailMem` flag word asks about the arena.
///
/// AROS's `AvailMem` reads `MEMF_LARGEST` and `MEMF_TOTAL` as the two
/// questions; anything else means "the total".
pub fn avail_query(flags: u32) -> Query {
    if flags & memf::LARGEST != 0 {
        Query::Largest
    } else {
        Query::Total
    }
}

/// Validate an `AllocMem`/`CreatePool` flag word.
///
/// Returns the flags on success, or [`errors::INVALID_VALUE`] when the word
/// asks for something contradictory. There is exactly one such word:
/// `MEMF_PUBLIC` and `MEMF_PRIVATE` name opposite ownership, and a
/// request that asks for both cannot be satisfied by any allocator.
///
/// Unknown bits are *not* rejected. AmigaOS grew flag bits over the years
/// and old programs passing new bits to old systems worked, so the same
/// tolerance runs in this direction too.
pub fn validate(flags: u32) -> Result<u32, i32> {
    if flags & memf::PUBLIC != 0 && flags & memf::PRIVATE != 0 {
        return Err(errors::INVALID_VALUE);
    }
    Ok(flags)
}

/// True when the allocation must be zero-filled.
pub const fn must_clear(flags: u32) -> bool {
    flags & memf::CLEAR != 0
}

/// True when the allocation must live in the low two gigabytes.
///
/// Every task's arena is in the low half of the address space on a 64-bit
/// machine, so the request is satisfied by construction — but the flag is
/// still *checked*, because a program that asks for it and is refused must
/// hear about it rather than discover the answer by faulting later.
pub const fn wants_low_memory(flags: u32) -> bool {
    flags & memf::THIRTY_ONE_BIT != 0
}

#[cfg(test)]
extern crate std;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_flag_values_are_the_aros_ones() {
        // Transcribed from AROS's `compiler/include/exec/memory.h`.
        assert_eq!(memf::ANY, 0);
        assert_eq!(memf::PUBLIC, 1);
        assert_eq!(memf::CHIP, 2);
        assert_eq!(memf::FAST, 4);
        assert_eq!(memf::VIRTUAL, 8);
        assert_eq!(memf::EXECUTABLE, 16);
        assert_eq!(memf::LOCAL, 1 << 8);
        assert_eq!(memf::TWENTY_FOUR_BIT_DMA, 1 << 9);
        assert_eq!(memf::KICK, 1 << 10);
        assert_eq!(memf::PRIVATE, 1 << 11);
        assert_eq!(memf::THIRTY_ONE_BIT, 1 << 12);
        assert_eq!(memf::CLEAR, 1 << 16);
        assert_eq!(memf::LARGEST, 1 << 17);
        assert_eq!(memf::REVERSE, 1 << 18);
        assert_eq!(memf::TOTAL, 1 << 19);
        assert_eq!(memf::NO_EXPUNGE, 1 << 31);
    }

    #[test]
    fn the_physical_mask_covers_the_hardware_flags() {
        assert_eq!(
            memf::PHYSICAL_MASK & memf::PRIVATE,
            0,
            "ownership is not a physical property"
        );
        assert_ne!(memf::PHYSICAL_MASK & memf::CHIP, 0);
        assert_ne!(memf::PHYSICAL_MASK & memf::FAST, 0);
    }

    #[test]
    fn a_public_private_allocation_is_contradictory() {
        assert_eq!(
            validate(memf::PUBLIC | memf::PRIVATE),
            Err(errors::INVALID_VALUE)
        );
        assert_eq!(validate(memf::PRIVATE), Ok(memf::PRIVATE));
        assert_eq!(validate(memf::PUBLIC), Ok(memf::PUBLIC));
    }

    #[test]
    fn validation_passes_the_classic_flag_words() {
        assert_eq!(
            validate(memf::CLEAR | memf::FAST),
            Ok(memf::CLEAR | memf::FAST)
        );
        assert_eq!(validate(memf::ANY), Ok(0));
    }

    #[test]
    fn unknown_bits_are_tolerated() {
        // AmigaOS grew flag bits over the years; a program written against a
        // newer header still allocates on an older system.
        assert_eq!(validate(1 << 20), Ok(1 << 20));
    }

    #[test]
    fn clear_is_the_zero_fill_request() {
        assert!(must_clear(memf::CLEAR));
        assert!(!must_clear(memf::FAST));
    }

    #[test]
    fn the_low_memory_request_is_recognised() {
        assert!(wants_low_memory(memf::THIRTY_ONE_BIT));
        assert!(!wants_low_memory(memf::FAST));
    }

    #[test]
    fn avail_mem_reads_the_largest_block_only_when_asked() {
        assert_eq!(avail_query(0), Query::Total);
        assert_eq!(avail_query(memf::TOTAL), Query::Total);
        assert_eq!(avail_query(memf::LARGEST), Query::Largest);
    }
}
