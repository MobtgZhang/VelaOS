//! The AArch64 exception vectors and the syndrome register.
//!
//! # The vector table
//!
//! AArch64 has one vector table, at `VBAR_EL1`, with sixteen entries: four
//! groups (current EL with SP0, current EL with SPx, lower EL with SP0,
//! lower EL with SPx), each with a synchronous, an IRQ, an FIQ and an
//! SError entry. Each slot is 0x200 bytes, so the whole table is 8 KiB, and
//! the entry for a particular exception is at a fixed offset within it.
//!
//! # The syndrome register
//!
//! `ESR_EL1` says what went wrong. Its `EC` field (bits 31:26) names the
//! exception class, and for a data abort the `ISS` field says whether the
//! fault was a translation fault, a permission fault, or something else —
//! and whether it was a read or a write.

/// The exception classes, `ESR_EL1.EC`.
pub mod ec {
    /// An exception taken from the same exception level.
    pub const SAME_EL: u8 = 0b000000;
    /// An exception taken from a lower exception level, running at EL0.
    pub const SP0: u8 = 0b000001;
    /// An exception taken from a lower exception level, running at EL1.
    pub const SP1: u8 = 0b000010;
    /// A supervisor call (`svc`).
    pub const SVC: u8 = 0b001011;
    /// A data abort from the current exception level.
    pub const DATA_ABORT_CURRENT: u8 = 0b100100;
    /// A data abort from a lower exception level.
    pub const DATA_ABORT_LOWER: u8 = 0b100101;
    /// A system register access to a lower exception level.
    pub const SYSTEM_REGISTER_LOWER: u8 = 0b100110;
    /// An instruction abort from the current exception level.
    pub const INSTRUCTION_ABORT_CURRENT: u8 = 0b100111;
    /// An instruction abort from a lower exception level.
    pub const INSTRUCTION_ABORT_LOWER: u8 = 0b101000;
    /// A PC alignment error.
    pub const PC_ALIGNMENT: u8 = 0b101010;
    /// A data abort from an unknown lower exception level.
    pub const DATA_ABORT_UNKNOWN: u8 = 0b101011;
}

/// The `ESR_EL1` shift for the exception class.
pub const ESR_EC_SHIFT: u32 = 26;

/// The `ESR_EL1` mask for the exception class.
pub const ESR_EC_MASK: u64 = 0x3F;

/// The `ESR_EL1` shift for the instruction syndrome.
pub const ESR_ISS_SHIFT: u32 = 0;

/// The data-abort translation fault codes, in `ESR_EL1.DFSC`.
pub mod dfsc {
    /// The address is not mapped.
    pub const TRANSLATION_LEVEL_0: u8 = 0b000000;
    /// The address is not mapped at level 1.
    pub const TRANSLATION_LEVEL_1: u8 = 0b000001;
    /// The address is not mapped at level 2.
    pub const TRANSLATION_LEVEL_2: u8 = 0b000010;
    /// The address is not mapped at level 3.
    pub const TRANSLATION_LEVEL_3: u8 = 0b000011;
    /// The access is not permitted at level 0.
    pub const PERMISSION_LEVEL_0: u8 = 0b001000;
    /// The access is not permitted at level 1.
    pub const PERMISSION_LEVEL_1: u8 = 0b001001;
    /// The access is not permitted at level 2.
    pub const PERMISSION_LEVEL_2: u8 = 0b001010;
    /// The access is not permitted at level 3.
    pub const PERMISSION_LEVEL_3: u8 = 0b001011;
}

/// The `ESR_EL1` field that says the fault was a write.
pub const ESR_WNDR: u64 = 1 << 6;

/// The exception class of a syndrome word.
pub const fn esr_class(esr: u64) -> u8 {
    ((esr >> ESR_EC_SHIFT) & ESR_EC_MASK) as u8
}

/// The instruction syndrome of a syndrome word.
pub const fn esr_iss(esr: u64) -> u64 {
    esr >> ESR_ISS_SHIFT
}

/// The data-fault status code of a syndrome word.
pub const fn esr_dfsc(esr: u64) -> u8 {
    (esr_iss(esr) & 0x3F) as u8
}

/// True when the data fault was a translation fault — the address is not
/// mapped — rather than a permission fault.
pub const fn is_translation_fault(esr: u64) -> bool {
    esr_dfsc(esr) < 0b001000
}

/// True when the data fault was a write.
pub const fn is_write_fault(esr: u64) -> bool {
    esr & ESR_WNDR != 0
}

/// The exception the syndrome describes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exception {
    /// A supervisor call: the syscall gate.
    SupervisorCall,
    /// A data abort from the current exception level.
    DataAbort {
        /// True when the fault was a write.
        write: bool,
        /// True when the address was not mapped rather than not permitted.
        translation: bool,
    },
    /// An instruction abort from the current exception level.
    InstructionAbort,
    /// A PC alignment error.
    PcAlignment,
    /// Anything else, carrying the raw class.
    Other(u8),
}

/// Decode a syndrome word.
pub const fn decode(esr: u64) -> Exception {
    match esr_class(esr) {
        ec::SVC => Exception::SupervisorCall,
        ec::DATA_ABORT_CURRENT | ec::DATA_ABORT_LOWER => Exception::DataAbort {
            write: is_write_fault(esr),
            translation: is_translation_fault(esr),
        },
        ec::INSTRUCTION_ABORT_CURRENT | ec::INSTRUCTION_ABORT_LOWER => Exception::InstructionAbort,
        ec::PC_ALIGNMENT => Exception::PcAlignment,
        other => Exception::Other(other),
    }
}

/// The offset of an entry in the vector table, for a vector spacing of
/// `0x200` bytes.
///
/// The sixteen entries are laid out in the order the architecture names
/// them: current EL with SP0, current EL with SPx, lower EL with SP0, lower
/// EL with SPx — and within each group, synchronous, IRQ, FIQ and SError.
pub const fn vector_offset(index: usize) -> u64 {
    (index as u64) * 0x200
}

/// The index of the synchronous entry for the current exception level with
/// the stack pointer in use.
pub const VECTOR_CURRENT_SP0_SYNC: usize = 0;

/// The index of the synchronous entry for the current exception level with
/// the *other* stack pointer.
pub const VECTOR_CURRENT_SPX_SYNC: usize = 1;

/// The index of the synchronous entry for a lower exception level.
pub const VECTOR_LOWER_SP0_SYNC: usize = 2;

/// The index of the synchronous entry for a lower exception level with the
/// other stack pointer.
pub const VECTOR_LOWER_SPX_SYNC: usize = 3;

#[cfg(test)]
extern crate std;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_exception_classes_are_the_architectures() {
        // A supervisor call is class 0b001011.
        assert_eq!(ec::SVC, 0b001011);
        // A data abort from the current level is 0b100100.
        assert_eq!(ec::DATA_ABORT_CURRENT, 0b100100);
        // The instruction abort from the current level is 0b100111.
        assert_eq!(ec::INSTRUCTION_ABORT_CURRENT, 0b100111);
        // The PC alignment error is 0b101010.
        assert_eq!(ec::PC_ALIGNMENT, 0b101010);
    }

    #[test]
    fn the_syndrome_decodes_a_supervisor_call() {
        let esr = (ec::SVC as u64) << ESR_EC_SHIFT;
        assert_eq!(esr_class(esr), ec::SVC);
        assert_eq!(decode(esr), Exception::SupervisorCall);
    }

    #[test]
    fn the_syndrome_decodes_a_translation_fault() {
        // A data abort, level-3 translation fault, from a read.
        let esr =
            (ec::DATA_ABORT_CURRENT as u64) << ESR_EC_SHIFT | (dfsc::TRANSLATION_LEVEL_3 as u64);
        assert_eq!(
            decode(esr),
            Exception::DataAbort {
                write: false,
                translation: true,
            }
        );
    }

    #[test]
    fn the_syndrome_decodes_a_permission_fault_and_a_write() {
        let esr = (ec::DATA_ABORT_LOWER as u64) << ESR_EC_SHIFT
            | (dfsc::PERMISSION_LEVEL_1 as u64)
            | ESR_WNDR;
        assert_eq!(
            decode(esr),
            Exception::DataAbort {
                write: true,
                translation: false,
            }
        );
    }

    #[test]
    fn the_vector_offsets_stride_by_the_spacing() {
        assert_eq!(vector_offset(0), 0);
        assert_eq!(vector_offset(1), 0x200);
        assert_eq!(vector_offset(15), 15 * 0x200);
    }

    #[test]
    fn the_vector_indices_are_the_architectures() {
        assert_eq!(VECTOR_CURRENT_SP0_SYNC, 0);
        assert_eq!(VECTOR_CURRENT_SPX_SYNC, 1);
        assert_eq!(VECTOR_LOWER_SP0_SYNC, 2);
        assert_eq!(VECTOR_LOWER_SPX_SYNC, 3);
    }
}
