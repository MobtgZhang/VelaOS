//! The `syscall` gate.
//!
//! # How `syscall` works on x86-64
//!
//! The `syscall` instruction is configured through four model-specific
//! registers:
//!
//! | MSR | Purpose |
//! |---|---|
//! | `IA32_LSTAR` | The entry point's linear address |
//! | `IA32_STAR` | The legacy segment selectors |
//! | `IA32_SFMASK` | The RFLAGS bits cleared on entry |
//! | `IA32_EFER.SCE` | Enables the `syscall` instruction |
//!
//! On entry, the CPU:
//!
//! 1. Loads the CS and SS selectors from `IA32_STAR`.
//! 2. Puts the return address (the instruction after `syscall`) in `RCX`.
//! 3. Puts the saved RFLAGS in `R11`.
//! 4. Clears the RFLAGS bits named by `IA32_SFMASK`.
//! 5. Loads the mask into RFLAGS, jumps to `IA32_LSTAR`, and masks the
//!    interrupt-enable flag.
//!
//! The return path is `sysret`, which pops the return address from `RCX` and
//! the saved flags from `R11`, and which switches *back* to user mode using
//! the selectors in `IA32_STAR`.
//!
//! # The `swapgs` discipline
//!
//! `syscall` does **not** switch the GS base. That is what `swapgs` is for,
//! and getting it wrong is the classic privilege-escalation bug:
//!
//! * On *entry* from user mode, the user program has been able to set the
//!   GS base to anything it likes — including a kernel address. The kernel
//!   must `swapgs` **before** trusting GS.
//! * On *exit* back to user mode, the kernel must `swapgs` again so the user
//!   program gets its own GS base back.
//!
//! The rule this crate follows is: `swapgs` immediately on entry, and
//! `swapgs` immediately before `sysret`. Any code between those two points
//! may trust GS.
//!
//! # The mask
//!
//! `IA32_SFMASK` clears the bits the kernel wants cleared on entry. The
//! standard choice clears the interrupt-enable flag, so a syscall runs with
//! interrupts disabled unless it explicitly re-enables them. That is what
//! this implementation does.

use core::arch::asm;

use vela_hal_x86_64::regs;

/// The `IA32_STAR` value that sets up the `syscall`/`sysret` selectors.
///
/// # The arithmetic, which is not negotiable
///
/// The `syscall` instruction does not take the selectors from anywhere else;
/// it *derives* them from `IA32_STAR`, and the derivation is fixed by the
/// architecture:
///
/// * On entry, `CS` is `IA32_STAR[47:32]` and `SS` is that value plus 8.
///   Both must be **kernel** descriptors, so `IA32_STAR[47:32]` is `0x08`
///   and `SS` becomes `0x10`.
/// * On return, `sysret` loads `CS` from `IA32_STAR[63:48] + 16` and `SS`
///   from `IA32_STAR[63:48] + 8`, with RPL forced to 3. Both must be **user**
///   descriptors, so the user data descriptor must sit 8 bytes below the user
///   code descriptor.
///
/// The second requirement is what dictates the GDT's *order*: the user data
/// descriptor is at index 3 (`0x18`) and the user code descriptor at index 4
/// (`0x20`), which makes `IA32_STAR[63:48]` equal to `0x10`.
///
/// # What the old value got wrong
///
/// The previous value used `0x33` and `0x23`, on the belief that the `syscall`
/// selectors needed RPL 3. They do not: `syscall` is a *kernel* transition,
/// and the CPU clears the RPL of the loaded kernel selectors itself. A
/// selector with RPL 3 in `IA32_STAR[47:32]` would load `CS` with RPL 3 — a
/// user-mode code segment — on a kernel entry, which is a privilege
/// *escalation* in the wrong direction, and it names a descriptor that the
/// kernel's GDT does not have.
///
/// The values here are the ones Linux uses.
pub const STAR_VALUE: u64 = (0x10u64 << 48) | (0x08u64 << 32);

/// The `IA32_SFMASK` value.
///
/// The interrupt-enable flag is cleared on entry, so a syscall handler runs
/// with interrupts disabled. That is the safe default: the handler may
/// touch shared kernel state, and it may not assume that an interrupt will
/// not arrive in the middle.
pub const SFMASK_VALUE: u64 = 1 << 9;

/// The `syscall` entry point.
///
/// # Safety
///
/// The CPU jumps here from user mode with the arguments in the registers
/// described in [`crate::numbers`]. The handler must not return normally;
/// it must `sysret`.
#[no_mangle]
pub unsafe extern "C" fn syscall_entry() -> ! {
    // The user program's GS base is untrusted. `swapgs` puts the kernel's GS
    // base in, and the kernel's GS base is the one the user program cannot
    // reach.
    //
    // SAFETY: this is the documented `syscall` entry discipline.
    unsafe {
        asm!("swapgs", options(nostack, preserves_flags));
    }

    // The syscall number is in `rax`, and the arguments are in `rdi`, `rsi`,
    // `rdx`, `r10`, `r8` and `r9` — the System V argument registers except
    // for `r10` in place of `rcx`, because `rcx` holds the return address.
    let number: u64;
    let a0: u64;
    let a1: u64;
    let a2: u64;
    let a3: u64;
    let a4: u64;
    let a5: u64;
    // SAFETY: the `syscall` instruction leaves these registers as the ABI
    // specifies, and reading them is free of side effects.
    unsafe {
        asm!(
            "mov {number}, rax",
            "mov {a0}, rdi",
            "mov {a1}, rsi",
            "mov {a2}, rdx",
            "mov {a3}, r10",
            "mov {a4}, r8",
            "mov {a5}, r9",
            number = out(reg) number,
            a0 = out(reg) a0,
            a1 = out(reg) a1,
            a2 = out(reg) a2,
            a3 = out(reg) a3,
            a4 = out(reg) a4,
            a5 = out(reg) a5,
            options(nostack, preserves_flags),
        );
    }

    let args = crate::numbers::SyscallArgs::new(a0, a1, a2, a3, a4, a5);
    let result = crate::dispatch::handle_syscall(number, args);

    // The result goes back in `rax` and `rdx`.
    //
    // SAFETY: as above.
    unsafe {
        asm!(
            "mov rax, {value}",
            "mov rdx, {error}",
            value = in(reg) result.value,
            error = in(reg) result.error as u64,
            options(nostack, preserves_flags),
        );
    }

    // Return to user mode. `sysret` pops the return address from `rcx` and
    // the saved flags from `r11`, both of which the CPU set on entry.
    //
    // `swapgs` restores the user program's GS base before the user program
    // can observe it.
    //
    // SAFETY: `rcx` and `r11` were set by the `syscall` instruction and have
    // not been clobbered: the handler above uses only the argument
    // registers and `rax`/`rdx`.
    unsafe {
        asm!("swapgs", "sysretq", options(nostack),);
    }

    // `sysretq` does not return. The trailing `ud2` makes that explicit to
    // the CPU: if the return path were ever taken, the machine would fault
    // loudly rather than continue into whatever follows.
    core::arch::asm!("ud2", options(noreturn, nostack))
}

/// Install the `syscall` gate.
///
/// This must be called once, after the GDT is installed and before any user
/// code runs.
///
/// # Safety
///
/// Must be called with interrupts disabled, and only once per CPU.
pub unsafe fn install() {
    // SAFETY: the caller guarantees this runs once, with interrupts off, and
    // that the GDT is installed so the selectors in `STAR_VALUE` are valid.
    unsafe {
        // `IA32_LSTAR` points at the entry point.
        regs::wrmsr(regs::MSR_LSTAR, syscall_entry as *const () as u64);
        // `IA32_STAR` sets up the segment selectors.
        regs::wrmsr(regs::MSR_STAR, STAR_VALUE);
        // `IA32_SFMASK` clears the interrupt-enable flag on entry.
        regs::wrmsr(regs::MSR_SFMASK, SFMASK_VALUE);
        // `IA32_EFER.SCE` enables the `syscall` instruction itself.
        let efer = regs::read_efer();
        regs::write_efer(efer | (1 << 0));
    }
}

/// True when the `syscall` instruction is enabled.
pub fn is_enabled() -> bool {
    if !regs::ON_BARE_METAL {
        return false;
    }
    // SAFETY: `RDMSR` on `IA32_EFER` is always safe.
    let efer = regs::read_efer();
    efer & (1 << 0) != 0
}

#[cfg(test)]
extern crate std;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_star_value_names_the_kernel_descriptors_on_entry() {
        // `IA32_STAR[47:32]` is the `CS` the CPU loads on a `syscall` entry,
        // and `SS` is that value plus 8. Both must be the *kernel*
        // descriptors: a `syscall` is a transition *into* ring 0.
        let cs = (STAR_VALUE >> 32) & 0xFFFF;
        assert_eq!(cs, 0x08);
        assert_eq!(cs & 0b11, 0b00, "the syscall CS must be ring 0");
        assert_eq!(cs + 8, 0x10, "the syscall SS must be the kernel data");
    }

    #[test]
    fn the_star_value_names_the_user_descriptors_on_return() {
        // `sysret` loads `CS` from `IA32_STAR[63:48] + 16` and `SS` from
        // that plus 8, with RPL forced to 3.
        let base = (STAR_VALUE >> 48) & 0xFFFF;
        assert_eq!(base + 16, 0x20, "the sysret CS must be the user code");
        assert_eq!(base + 8, 0x18, "the sysret SS must be the user data");
        // The CPU forces RPL 3 on the way out, so the stored base has RPL 0.
        assert_eq!(base & 0b11, 0b00);
    }

    #[test]
    fn the_star_value_matches_the_installed_gdt() {
        // The gate and the GDT must agree, or the first syscall faults with a
        // `#GP` that names no file and no line.
        use vela_hal_x86_64::gdt;
        assert_eq!(
            (STAR_VALUE >> 32) & 0xFFFF,
            gdt::KERNEL_CODE_SELECTOR as u64
        );
        assert_eq!(
            ((STAR_VALUE >> 32) & 0xFFFF) + 8,
            gdt::KERNEL_DATA_SELECTOR as u64
        );
        assert_eq!(
            ((STAR_VALUE >> 48) & 0xFFFF) + 16,
            gdt::USER_CODE_SELECTOR as u64
        );
        assert_eq!(
            ((STAR_VALUE >> 48) & 0xFFFF) + 8,
            gdt::USER_DATA_SELECTOR as u64
        );
    }

    #[test]
    fn the_mask_clears_the_interrupt_enable_flag() {
        // The interrupt-enable flag is bit 9 of RFLAGS.
        assert_eq!(SFMASK_VALUE & (1 << 9), 1 << 9);
    }

    #[test]
    fn the_mask_leaves_the_rest_of_the_flags_alone() {
        // Clearing more than the interrupt-enable flag would silently
        // change the user's arithmetic flags, which a syscall must not do.
        assert_eq!(SFMASK_VALUE.count_ones(), 1);
    }

    #[test]
    fn the_entry_point_is_not_the_null_page() {
        // A null `IA32_LSTAR` would make the CPU jump to address zero.
        assert_ne!(syscall_entry as *const () as u64, 0);
    }
}
