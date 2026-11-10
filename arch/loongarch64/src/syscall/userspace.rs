//! Ring-3 entry: the `PRMD`/`CRMD` builders and the `ertn` transition.
//!
//! This module owns the LoongArch64-specific *mechanism* for entering user
//! mode. The kernel's [`crate::syscall::gate`] owns the syscall gate; this
//! module owns the initial drop into ring 3.
//!
//! # Why there is no frame to build
//!
//! x86-64 enters ring 3 by executing `iretq`, which pops a five-word frame
//! the kernel had to lay out byte-exactly on a stack. LoongArch64's `ertn`
//! reads everything from CSRs: it copies `PRMD.PPLV` → `CRMD.PLV`,
//! `PRMD.PIE` → `CRMD.IE`, and jumps to `ERA`. The "context" for a user
//! transition is therefore just two CSR writes and a stack-pointer swap —
//! no memory frame at all, and nothing for a hostile user stack to corrupt
//! on the way in. [`enter_user`] takes the entry point and stack top as
//! plain arguments for that reason; [`user_context`] exists only to keep
//! the x86-shaped surface (`context` value passed to `enter_user`) available
//! to callers that want it.
//!
//! # What the kernel must have arranged first
//!
//! * `CRMD.PG` set (paging on) — user VAs are meaningless otherwise, and
//!   `ertn` does not touch `PG`.
//! * `CRMD.DATF`/`DATL` at the coherent-cached encoding (see
//!   [`crmd_value_for_user_return`]) so a stray access while still in
//!   ring 0 behaves.
//! * The user's address space active (`CSR.PGDL`, see
//!   [`crate::addrspace::activate`]).
//! * `CSR.EUEN.FPE` if the program may use the FPU; disabled FPU is a
//!   trap (`FPDIS`, EC 15), not a silent fallback.

use crate::regs;

/// Build a `PRMD` word for a `ertn` into privilege level `plv` with the
/// interrupt enable `ie` (saved `PIE` → `CRMD.IE`).
///
/// `PRMD.PPLV` is bits 1:0 and `PRMD.PIE` is bit 2 (the `PWE` bit at bit 3
/// is the write-enable for llbit and stays clear).
pub const fn prmd_value(plv: u8, ie: bool) -> u64 {
    // `PLV` is two bits; a wider value would run into `PIE`.
    let plv = plv & 0b11;
    plv as u64 | ((ie as u64) << 2)
}

/// The `PRMD` word for the first entry into ring 3: PLV 3, interrupts
/// enabled.
pub const PRMD_USER: u64 = prmd_value(3, true);

/// The `CRMD` word a kernel context should run with while it returns to user
/// mode (and, identically, the kernel's steady-state configuration):
///
/// * `PLV = 0`, `IE = 0` — the return path itself runs privileged and
///   interrupted-off; `ertn` installs the user's values from `PRMD`,
/// * `DA = 0`, `PG = 1` — paging on,
/// * `DATF`/`DATL = 01` — coherent-cached behaviour for the (normally
///   unreachable) direct-address mode.
pub fn crmd_value_for_user_return() -> u64 {
    // PG = bit 4; DATF = bits 6:5 = 01; DATL = bits 8:7 = 01.
    1 << 4 | 0b01 << 5 | 0b01 << 7
}

/// Lay out the `context` value for the first entry into ring 3.
///
/// On LoongArch this is not a stack frame — see the module docs — so the
/// context simply packs the two arguments `enter_user` needs: the entry
/// point in the low half, the user stack top in the high half. It exists so
/// callers can keep the x86-shaped `user_context` → `enter_user(context)`
/// flow.
pub fn user_context(entry: u64, stack_top: u64) -> UserContext {
    debug_assert_eq!(stack_top & 0xF, 0, "user stack must be 16-byte aligned");
    UserContext { entry, stack_top }
}

/// The entry point and the stack a user program starts with.
///
/// The two are kept as two fields rather than packed into one word: a
/// user virtual address on this architecture is 48 bits wide, and packing
/// an address and a stack pointer into sixty-four bits would lose the top
/// half of both.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UserContext {
    /// The address the program starts at.
    pub entry: u64,
    /// The top of its stack. The page *at* the top is deliberately
    /// unmapped, as on the other architectures, so a stack overflow faults
    /// instead of quietly running into whatever is above.
    pub stack_top: u64,
}

/// Drop to ring 3.
///
/// # Safety
///
/// The caller must be running in ring 0 with interrupts disabled, `CRMD.PG`
/// set, and the target program's address space active. `entry` must be a
/// mapped user VA; `stack_top` the top of a mapped user stack (the page *at*
/// the top deliberately unmapped, as on the other architectures). The
/// instruction does not return.
pub unsafe fn enter_user(entry: u64, stack_top: u64) -> ! {
    #[cfg(target_arch = "loongarch64")]
    {
        // SAFETY: the caller guarantees the entry point and stack are mapped
        // user VAs in the active address space, and that `CRMD` holds
        // [`crmd_value_for_user_return`]. `ertn` then installs `PRMD` (ring
        // 3, interrupts on) and jumps to `ERA`.
        unsafe {
            core::arch::asm!(
                "csrwr {era}, {era_csr}",
                "ori {prmd}, $r0, {prmd_user}",
                "csrwr {prmd}, {prmd_csr}",
                "or $r3, {sp}, $r0",
                "ertn",
                era = in(reg) entry,
                sp = in(reg) stack_top,
                prmd = out(reg) _,
                era_csr = const regs::CSR_ERA,
                prmd_csr = const regs::CSR_PRMD,
                prmd_user = const PRMD_USER,
                options(nomem, nostack)
            );
        }
        unreachable!("ertn does not return");
    }
    #[cfg(not(target_arch = "loongarch64"))]
    {
        let _ = entry;
        let _ = stack_top;
        // Hosted builds cannot enter ring 3; the entry function is only
        // reachable from the scheduler on bare metal. The wait is a *pause*
        // rather than a spin, so a hosted test does not burn a core.
        loop {
            core::hint::spin_loop();
        }
    }
}

/// The current privilege level, for diagnostics and tests: 0 in the kernel,
/// 3 in user mode.
pub fn current_cpl() -> u8 {
    if !regs::ON_BARE_METAL {
        return 0;
    }
    // SAFETY: reading `CRMD` does not change state.
    (unsafe { regs::read_csr::<{ regs::CSR_CRMD }>() } & 0b11) as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prmd_encodes_plv_and_pie() {
        assert_eq!(prmd_value(3, true), 0b111, "PLV 3 + PIE");
        assert_eq!(prmd_value(3, false), 0b011);
        assert_eq!(prmd_value(0, true), 0b100);
        assert_eq!(prmd_value(0, false), 0);
        assert_eq!(PRMD_USER, 0b111);
    }

    #[test]
    fn crmd_has_paging_and_cached_dat() {
        let value = crmd_value_for_user_return();
        assert_eq!(value & 0b11, 0, "PLV 0");
        assert_eq!(value & 0b100, 0, "IE clear during the return path");
        assert_eq!(value & 1 << 4, 1 << 4, "PG set");
        assert_eq!((value >> 5) & 0b11, 0b01, "DATF coherent cached");
        assert_eq!((value >> 7) & 0b11, 0b01, "DATL coherent cached");
    }

    #[test]
    fn user_context_carries_the_entry_and_the_stack() {
        // Both are 48-bit addresses, and neither loses a bit.
        let ctx = user_context(0x0000_1000_0000_0000, 0x0000_2000_0000_0000);
        assert_eq!(ctx.entry, 0x0000_1000_0000_0000);
        assert_eq!(ctx.stack_top, 0x0000_2000_0000_0000);
    }

    #[test]
    fn hosted_cpl_is_kernel() {
        if !regs::ON_BARE_METAL {
            assert_eq!(current_cpl(), 0);
        }
    }
}
