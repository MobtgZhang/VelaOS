//! Ring-3 entry: the S-mode → U-mode ERET payload and `enter_user`.
//!
//! # What an ERET into user mode actually is
//!
//! There is no single "enter user mode" instruction on RISC-V. Dropping from
//! S-mode to U-mode is the *return* half of a trap, performed by `sret`, and
//! the hardware takes the details from two CSRs plus whatever the general
//! registers hold at that moment:
//!
//! | Where | Value | Meaning |
//! |---|---|---|
//! | `sepc` | `entry` | the first user instruction's address |
//! | `sstatus` | [`SSTATUS_FOR_USER`] | `SPP`=0 (return to U-mode), `SPIE`=1 (interrupts on afterwards), `UXL`=2 (64-bit user mode) |
//! | `satp` | `satp` | the user address space, written *before* the fence |
//! | `sp` | `sp` | the user stack pointer, loaded by the glue |
//!
//! Unlike the x86-64 `iretq` frame, nothing lives on the stack: `sret`
//! consults CSRs only, so the glue below writes the CSRs and then loads the
//! user `sp` — the one register `sret` does not restore — before returning.
//! The argument registers `a0`–`a7` and `ra` are zeroed so the first user
//! instruction starts from a determinate state.
//!
//! # Ordering that is not optional
//!
//! The `satp` write must be fenced *after* with `sfence.vma` (privileged
//! spec, "satp"): an `sfence.vma` after the register write guarantees the
//! subsequent instruction fetch uses the new tables. Issuing user code
//! before the fence has completed would run it under the old — or no —
//! translation.

use crate::syscall::sstatus_value_for_user_return;

/// The `sstatus` value `enter_user` writes before its `sret`.
///
/// Kept as a named constant so the payload is inspectable on the host; the
/// bit reasoning lives in [`crate::syscall::sstatus_value_for_user_return`].
pub const SSTATUS_FOR_USER: u64 = sstatus_value_for_user_return();

/// The initial register state of a freshly entered user program.
///
/// RISC-V's `sret` restores only the program counter and privilege level,
/// so this table *is* the initial state, and every field of it is the
/// glue's responsibility:
///
/// | Register | Initial value | Why |
/// |---|---|---|
/// | `pc` | `entry` | via `sepc` |
/// | `sp` | `sp` | the user stack pointer |
/// | `a0`–`a7`, `ra` | 0 | determinate start; an argument-bearing start belongs to a future `main` ABI |
/// | `gp`, `tp` | 0 | the user image re-establishes them if it uses them |
/// | `t0`–`t6`, `s0`–`s11` | unspecified | never read before the program writes them |
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UserEntry {
    /// The first instruction to execute.
    pub entry: u64,
    /// The user stack pointer.
    pub sp: u64,
    /// The `satp` value for the user address space.
    pub satp: u64,
}

impl UserEntry {
    /// Validate the payload's arithmetic before any privileged state is
    /// touched: `entry` and `sp` must be canonical Sv39 addresses, and the
    /// `satp` value must name Sv39 mode.
    pub const fn is_valid(&self) -> bool {
        crate::paging::is_canonical(self.entry)
            && crate::paging::is_canonical(self.sp)
            && crate::paging::satp_mode(self.satp) == crate::paging::SATP_MODE_SV39
    }
}

/// Drop to user mode at `entry`, on stack `sp`, under address space `satp`.
///
/// The order below is the spec's: write `satp`, fence the translation caches,
/// write `sepc` and `sstatus`, load the user `sp` — and only then `sret`,
/// which the hardware turns into a U-mode fetch of `sepc`.
///
/// # Safety
///
/// * The payload must satisfy [`UserEntry::is_valid`] — the glue does not
///   re-check it, because a fault here would be a bug in the caller.
/// * Every page `entry` and `sp` name must be mapped in the `satp` space,
///   user-accessible (`U` set), and `entry` executable.
/// * The trap machinery must be installed ([`crate::trap::install`] and
///   [`crate::trap::set_trap_stack`]) before user mode runs, or the first
///   syscall or interrupt has nowhere to land.
pub unsafe fn enter_user(payload: UserEntry) -> ! {
    #[cfg(not(target_arch = "riscv64"))]
    let _ = payload;
    #[cfg(target_arch = "riscv64")]
    // SAFETY: the caller guarantees the payload is valid and mapped.
    unsafe {
        core::arch::asm!(
            "csrw satp, {satp}",
            "sfence.vma zero, zero",
            "csrw sepc, {entry}",
            "csrw sstatus, {sstatus}",
            "mv sp, {sp}",
            "mv ra, zero",
            "mv a0, zero",
            "mv a1, zero",
            "mv a2, zero",
            "mv a3, zero",
            "mv a4, zero",
            "mv a5, zero",
            "mv a6, zero",
            "mv a7, zero",
            "mv gp, zero",
            "mv tp, zero",
            "sret",
            satp = in(reg) payload.satp,
            entry = in(reg) payload.entry,
            sstatus = in(reg) SSTATUS_FOR_USER,
            sp = in(reg) payload.sp,
            options(nostack, noreturn)
        );
    }
    #[cfg(not(target_arch = "riscv64"))]
    {
        // Off the target the transition cannot be performed; park instead of
        // pretending to have entered user mode.
        loop {
            core::hint::spin_loop();
        }
    }
}

#[cfg(test)]
extern crate std;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paging::{satp_mode, satp_value, SATP_MODE_SV39};

    #[test]
    fn the_sret_payload_is_spp_clear_spie_set() {
        // SPP=0 is what makes sret land in U-mode; there is no separate
        // "user bit" to set.
        assert_eq!(SSTATUS_FOR_USER & (1 << 8), 0);
        assert_ne!(SSTATUS_FOR_USER & (1 << 5), 0);
    }

    #[test]
    fn a_well_formed_payload_validates() {
        let payload = UserEntry {
            entry: 0x0000_0000_0040_1000,
            sp: 0x0000_0000_0007_FFFF_0000,
            satp: satp_value(0x8000, 0),
        };
        assert!(payload.is_valid());
    }

    #[test]
    fn a_non_canonical_entry_is_rejected() {
        // Bit 38 set without the sign extension: the hardware would page
        // fault on the very first fetch.
        let payload = UserEntry {
            entry: 0x0000_0000_0080_0000_1000,
            sp: 0x1000,
            satp: satp_value(0x8000, 0),
        };
        assert!(!payload.is_valid());
    }

    #[test]
    fn a_non_sv39_satp_is_rejected() {
        // Mode 0 is bare-metal translation: no user mapping exists under it.
        let payload = UserEntry {
            entry: 0x1000,
            sp: 0x2000,
            satp: 0x8000, // mode 0, not 8
        };
        assert_ne!(satp_mode(payload.satp), SATP_MODE_SV39);
        assert!(!payload.is_valid());
    }

    #[test]
    fn the_user_stack_lives_in_the_low_half() {
        // The high half is the kernel window; a user `sp` above it would be
        // a kernel address handed to a program.
        let payload = UserEntry {
            entry: 0x1000,
            sp: 0x0000_0000_0007_FFFF_0000,
            satp: satp_value(0x8000, 0),
        };
        assert!(payload.sp < crate::paging::KERNEL_VIRTUAL_BASE);
    }
}
