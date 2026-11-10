//! The ecall-based syscall gate for RV64.
//!
//! # The calling convention, and why it is the Linux one
//!
//! RISC-V has no dedicated `syscall` instruction: a system call is an
//! `ecall` from U-mode, which raises exception cause 8 ("environment call
//! from U-mode") and traps to `stvec`. That leaves the register convention
//! entirely to software, and every RISC-V userland that matters — Linux,
//! musl, glibc — has settled on the same one:
//!
//! | Register | Meaning |
//! |---|---|
//! | `a7` | the syscall number |
//! | `a0`–`a5` | the six syscall arguments |
//! | `a0` (on return) | the result |
//! | `a1` (on return) | the error flag |
//!
//! VelaOS mirrors it exactly, so a program built against the RISC-V Linux
//! convention works unchanged against the VelaOS gate, and the SDK documents
//! the same numbers for both. The trap-entry stub in [`crate::trap`] reads
//! the cause, sees 8, passes `a0`–`a5` through and moves the number from
//! `a7` into the seventh argument slot (`a6`) for the dispatcher call.
//!
//! # Mechanism vs policy
//!
//! As with the x86-64 gate, this crate owns only the mechanism: the register
//! contract and the result/error shape. Which syscall number does what is
//! the kernel's dispatch table, supplied as the `vela_syscall_dispatch`
//! function the stub calls.

use crate::regs::{self, SSTATUS_SPIE, SSTATUS_SPP, SSTATUS_UXL_64};

/// The `scause` exception code that routes a trap into the syscall gate:
/// "environment call from U-mode" (privileged spec, `scause` table).
pub const ECALL_FROM_UMODE: u64 = 8;

/// The result of a syscall, as the gate hands it back to user mode.
///
/// # Why a two-field struct
///
/// The psABI returns a two-word aggregate in `a0`/`a1` — the same registers
/// the convention dedicates to result and error — so a `#[repr(C)]` pair is
/// exactly what the trap stub sees in those registers after the dispatch
/// call returns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub struct SyscallResult {
    /// The syscall's return value, delivered to user mode in `a0`.
    pub result: u64,
    /// Non-zero when the syscall failed, delivered in `a1`.
    pub error: u64,
}

/// Build the `sstatus` value an `sret` into U-mode must be written with.
///
/// Three fields decide what the `sret` does (privileged spec, "sstatus" and
/// "sret"):
///
/// * `SPP` (bit 8) = **0**: return to U-mode. There is no dedicated "user
///   bit" in `sstatus`; the privilege level an `sret` enters is chosen
///   entirely by this field.
/// * `SPIE` (bit 5) = **1**: after the `sret` pops it into `SIE`, supervisor
///   interrupts are enabled — user code must be interruptible or the timer
///   that preempts it would never fire.
/// * `UXL` (bits 33:32) = **2** (`XLEN-64`): user mode executes 64-bit
///   instructions. On RV64 this is mandatory for 64-bit userland; the
///   encoding is the `misa.MXL` one (1 = 32-bit, 2 = 64-bit).
///
/// Everything else is zeroed: `SUM`/`MXR` start clear (the kernel sets them
/// transiently when it inspects user memory), and the FS/VXS float-state
/// fields are left for the FPU-enabling milestone.
pub const fn sstatus_value_for_user_return() -> u64 {
    // SPP = 0 by omission; SPIE and UXL are set.
    SSTATUS_SPIE | SSTATUS_UXL_64
}

/// The `sstatus` value with `SPP` set: an `sret` would return to S-mode.
///
/// Used by the kernel to inspect or build supervisor-return frames; the
/// trap stub never writes it directly.
pub const fn sstatus_spp_s() -> u64 {
    SSTATUS_SPP
}

/// The `sstatus.SPIE` bit on its own.
pub const fn sstatus_spie() -> u64 {
    SSTATUS_SPIE
}

/// The `sie` value that unmasks the supervisor timer interrupt only.
///
/// Bit 5 (`STIE`) is the enable that lets a timer expiry — delivered through
/// `sip.STIP`, typically written by the SBI or the `stimecmp` hardware —
/// trap into the kernel. See [`crate::timer`].
pub const fn sie_timer() -> u64 {
    regs::SIE_STIE
}

/// Read `sip`, the interrupt-pending register.
///
/// Returns 0 off bare metal. A pending bit at position *n* corresponds to
/// the same-numbered enable in `sie`; the timer's is bit 5.
pub fn read_sip() -> u64 {
    if !regs::ON_BARE_METAL {
        return 0;
    }
    // The value is declared for the target only: on a host build the
    // instruction is compiled out, and the function answers zero.
    #[cfg(target_arch = "riscv64")]
    let mut value: u64 = 0;
    #[cfg(not(target_arch = "riscv64"))]
    let value: u64 = 0;
    // SAFETY: reading `sip` has no side effects.
    #[cfg(target_arch = "riscv64")]
    unsafe {
        core::arch::asm!("csrr {0}, sip", out(reg) value);
    }
    value
}

/// True when the supervisor timer interrupt is pending on this hart.
pub fn timer_pending() -> bool {
    read_sip() & regs::SIE_STIE != 0
}

#[cfg(test)]
extern crate std;

#[cfg(test)]
mod tests {
    use super::*;
    use core::mem::{offset_of, size_of};

    #[test]
    fn the_user_return_value_is_spp_clear_spie_set_uxl_sixtyfour() {
        let value = sstatus_value_for_user_return();
        assert_eq!(value & SSTATUS_SPP, 0, "SPP=0: sret enters U-mode");
        assert_ne!(value & SSTATUS_SPIE, 0, "SPIE=1: interrupts re-enable");
        assert_eq!(value >> 32 & 0b11, 0b10, "UXL=2: 64-bit user mode");
        // SUM/MXR stay clear: the kernel opts in only while it inspects
        // user memory.
        assert_eq!(value & (1 << 18 | 1 << 19), 0);
    }

    #[test]
    fn spp_s_selects_supervisor_return() {
        assert_eq!(sstatus_spp_s(), 1 << 8);
        // The two returns differ: one comes back to S-mode, the other to
        // U-mode, and the difference is the `SPP` bit.
        assert_ne!(sstatus_spp_s(), sstatus_value_for_user_return());
    }

    #[test]
    fn the_timer_enable_is_bit_five() {
        assert_eq!(sie_timer(), 1 << 5);
    }

    #[test]
    fn the_ecall_cause_is_exception_eight() {
        // The cause is 8, an exception: the MSB is clear, which is what
        // the trap stub tests with `bltz`.
        assert_eq!(ECALL_FROM_UMODE, 8);
        assert_eq!(
            ECALL_FROM_UMODE & (1 << 63),
            0,
            "an exception, not an interrupt"
        );
    }

    #[test]
    fn the_result_struct_lands_in_a0_and_a1() {
        // Two eight-byte fields, C-repr: the psABI passes exactly this
        // shape back through the a0/a1 register pair.
        assert_eq!(size_of::<SyscallResult>(), 16);
        assert_eq!(offset_of!(SyscallResult, result), 0);
        assert_eq!(offset_of!(SyscallResult, error), 8);
    }

    #[test]
    fn sip_reads_zero_off_bare_metal() {
        assert!(!timer_pending());
        assert_eq!(read_sip(), 0);
    }
}
