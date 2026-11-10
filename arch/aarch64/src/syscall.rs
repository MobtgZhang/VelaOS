//! The AArch64 syscall gate and the return to user mode.
//!
//! # The shape of a system call
//!
//! A user program executes `svc #0`. The hardware takes the exception to
//! EL1 with the exception class `SVC` (0b0010_1011 = 0x15), and the
//! kernel's entry stub hands the registers to the dispatcher:
//!
//! | register | meaning |
//! |---|---|
//! | `x8` | the syscall number |
//! | `x0`–`x5` | the arguments |
//! | `x0` | the result |
//! | `x1` | the error |
//!
//! The split of the result and the error into two registers is the same
//! contract the x86-64 gate uses (`rax` and `rdx`), so a call reads the
//! same whichever architecture it is on.
//!
//! # Returning to user mode
//!
//! The exception return (`eret`) restores the program counter from `ELR_EL1`
//! and the saved status from `SPSR_EL1`. The kernel therefore sets `ELR_EL1`
//! to the address the program should resume at and `SPSR_EL1` to "return to
//! EL0 with interrupts enabled", and executes `eret`.

pub mod gate {
    //! The register contract of the syscall gate.

    /// The register that carries the syscall number.
    pub const NR_REG: usize = 8;
    /// The first register that carries an argument.
    pub const ARG0_REG: usize = 0;
    /// The last register that carries an argument.
    pub const ARG5_REG: usize = 5;
    /// The register that carries the result.
    pub const RESULT_REG: usize = 0;
    /// The register that carries the error.
    pub const ERROR_REG: usize = 1;

    /// The result of a syscall: a value and an error.
    ///
    /// The two halves are separate because an AmigaOS-style API returns a
    /// pointer on success and `NULL` on failure, and a single return value
    /// could not say which of the two a zero was.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    #[repr(C)]
    pub struct SyscallResult {
        /// The value the call produced.
        pub value: u64,
        /// The error, or zero for success.
        pub error: i32,
    }

    impl SyscallResult {
        /// A successful result.
        pub const fn ok(value: u64) -> Self {
            Self { value, error: 0 }
        }

        /// A failed result.
        pub const fn err(error: i32) -> Self {
            Self { value: 0, error }
        }

        /// True when the call succeeded.
        pub const fn is_ok(&self) -> bool {
            self.error == 0
        }

        /// The value, or zero on failure.
        pub const fn value_or_zero(&self) -> u64 {
            if self.is_ok() {
                self.value
            } else {
                0
            }
        }
    }
}

pub mod userspace {
    //! Returning to user mode.

    /// The `SPSR_EL1.M` field for EL0 with interrupts enabled.
    ///
    /// The mode field is bits 3:0: `0b0000` is EL0t (user, with the stack
    /// pointer taken from `SP_EL0`), `0b0101` is EL1h.
    pub const MODE_EL0T: u64 = 0b0000;
    /// The `SPSR_EL1.I` bit: interrupts enabled on return.
    pub const SPSR_I: u64 = 1 << 7;

    /// The `SPSR_EL1` value for a return to user mode.
    pub const fn spsr_for_user_return() -> u64 {
        MODE_EL0T | SPSR_I
    }

    /// The `SPSR_EL1` value for a return to the kernel.
    ///
    /// A trap that must not come back to user space — a fatal exception, a
    /// double fault — returns with the mode field naming EL1h.
    pub const fn spsr_for_kernel_return() -> u64 {
        0b0101
    }

    /// The `ELR_EL1` value for a return to `entry`.
    pub const fn elr_for(entry: u64) -> u64 {
        entry
    }
}

/// The trap-entry contract: the dispatcher the assembly calls.
///
/// # Safety
///
/// Called from the trap entry with the user program's registers. It must not
/// return into user space directly — it returns a [`gate::SyscallResult`],
/// and the entry stub puts the two halves in `x0` and `x1`.
pub type SyscallDispatch = unsafe extern "C" fn(
    x0: u64,
    x1: u64,
    x2: u64,
    x3: u64,
    x4: u64,
    x5: u64,
    nr: u64,
) -> gate::SyscallResult;

#[cfg(test)]
extern crate std;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_gate_uses_the_linux_register_convention() {
        // The number is in `x8`, the arguments in `x0`–`x5`, and the result
        // and the error in `x0` and `x1`.
        assert_eq!(gate::NR_REG, 8);
        assert_eq!(gate::ARG0_REG, 0);
        assert_eq!(gate::ARG5_REG, 5);
        assert_eq!(gate::RESULT_REG, 0);
        assert_eq!(gate::ERROR_REG, 1);
    }

    #[test]
    fn a_result_carries_a_value_and_an_error() {
        let ok = gate::SyscallResult::ok(42);
        assert!(ok.is_ok());
        assert_eq!(ok.value_or_zero(), 42);
        let failed = gate::SyscallResult::err(-2);
        assert!(!failed.is_ok());
        assert_eq!(failed.value_or_zero(), 0);
    }

    #[test]
    fn the_return_to_user_mode_sets_the_mode_field() {
        let spsr = userspace::spsr_for_user_return();
        assert_eq!(spsr & 0xF, userspace::MODE_EL0T, "EL0t");
        assert_ne!(spsr & userspace::SPSR_I, 0, "interrupts are enabled");
        // A trap that must not return to user mode names EL1h instead.
        assert_eq!(userspace::spsr_for_kernel_return() & 0xF, 0b0101);
    }

    #[test]
    fn the_return_address_is_the_entry_point() {
        assert_eq!(userspace::elr_for(0x4000_0000), 0x4000_0000);
    }
}
