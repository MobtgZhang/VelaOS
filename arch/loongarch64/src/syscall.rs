//! The LoongArch64 syscall gate and the return to user mode.
//!
//! # The shape of a system call
//!
//! A user program executes the `syscall` instruction. The hardware raises
//! an exception with `ESTAT.EC` = `SYS` (0x0B), and the kernel's trap entry
//! — see [`crate::idt`] — hands the registers to the dispatcher:
//!
//! | register | meaning |
//! |---|---|
//! | `a7` (r11) | the syscall number |
//! | `a0`–`a5` (r4–r9) | the arguments |
//! | `a0` (r4) | the result |
//! | `a1` (r5) | the error |
//!
//! The split of the result and the error into two registers is the same
//! contract the x86-64 gate uses (`rax` and `rdx`), so a call reads the
//! same whichever architecture it is on.
//!
//! # Returning to user mode
//!
//! The `ertn` instruction restores the program counter from `ERA` and the
//! privilege level from `PRMD.PPLV`. The kernel therefore sets `ERA` to the
//! address the program should resume at, `PRMD` to "return to user mode with
//! interrupts enabled", and executes `ertn`.

pub mod gate {
    //! The register contract of the syscall gate.

    /// The register that carries the syscall number.
    pub const NR_REG: usize = 11;
    /// The first register that carries an argument.
    pub const ARG0_REG: usize = 4;
    /// The last register that carries an argument.
    pub const ARG5_REG: usize = 9;
    /// The register that carries the result.
    pub const RESULT_REG: usize = 4;
    /// The register that carries the error.
    pub const ERROR_REG: usize = 5;

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

    /// `PRMD.PPLV`: the privilege level to return to.
    ///
    /// Level 0 is the kernel, level 3 is the user program.
    pub const PPLV_USER: u64 = 3;
    /// `PRMD.PIE`: re-enable interrupts on the return.
    pub const PRMD_PIE: u64 = 1 << 0;
    /// `CRMD.PLV`: the privilege level the processor runs at.
    pub const CRMD_PLV: u64 = 1 << 0;

    /// The `PRMD` value for a return to user mode.
    ///
    /// The user level is three, so `PPLV` is bits 2:0 and `PIE` is bit 0
    /// of the *previous* level's flags — the field layout is `PPLV[2:0]`
    /// followed by `PIE[0]`, `PIL[1]`, `PIE[2]` and the rest.
    pub const fn prmd_for_user_return() -> u64 {
        (PPLV_USER & 0x7) | (PRMD_PIE << 3)
    }

    /// The `CRMD` value with the processor running at user level.
    ///
    /// The kernel sets `CRMD.PLV` to zero before it executes `ertn`, so the
    /// processor comes back at level three.
    pub const fn crmd_for_user_return() -> u64 {
        0
    }

    /// The `ERA` value for a return to `entry`.
    pub const fn era_for(entry: u64) -> u64 {
        entry
    }
}

/// The trap-entry contract: the dispatcher the assembly calls.
///
/// # Safety
///
/// Called from the trap entry with the user program's registers. It must not
/// return into user space directly — it returns a [`gate::SyscallResult`],
/// and the entry stub puts the two halves in `a0` and `a1`.
pub type SyscallDispatch = unsafe extern "C" fn(
    a0: u64,
    a1: u64,
    a2: u64,
    a3: u64,
    a4: u64,
    a5: u64,
    nr: u64,
) -> gate::SyscallResult;

#[cfg(test)]
extern crate std;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_gate_uses_the_linux_register_convention() {
        // The number is in `a7`, the arguments in `a0`–`a5`, and the result
        // and the error in `a0` and `a1`.
        assert_eq!(gate::NR_REG, 11);
        assert_eq!(gate::ARG0_REG, 4);
        assert_eq!(gate::ARG5_REG, 9);
        assert_eq!(gate::RESULT_REG, 4);
        assert_eq!(gate::ERROR_REG, 5);
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
    fn the_return_to_user_mode_sets_the_privilege_level() {
        let prmd = userspace::prmd_for_user_return();
        assert_eq!(prmd & 0x7, userspace::PPLV_USER, "PPLV is three");
        assert_ne!(prmd & (userspace::PRMD_PIE << 3), 0, "PIE is set");
        assert_eq!(userspace::crmd_for_user_return() & userspace::CRMD_PLV, 0);
    }

    #[test]
    fn the_return_address_is_the_entry_point() {
        assert_eq!(userspace::era_for(0x4000_0000), 0x4000_0000);
    }
}
