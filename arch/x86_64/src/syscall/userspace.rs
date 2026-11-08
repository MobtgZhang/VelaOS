//! Ring-3 entry: the `iretq` frame, `enter_user`, and the GDT/TSS install.
//!
//! This module owns the x86-64-specific *mechanism* for entering user mode:
//! laying out the `iretq` frame, dropping to ring 3, and installing the GDT
//! and TSS that make ring 3 possible. The kernel's [`crate::gate`] owns the
//! `syscall` gate; this module owns the initial `iretq` transition.

use crate::regs;

/// Build the initial user context for a program.
///
/// The context is what `iretq` consumes: it is the frame the CPU builds as it
/// drops to ring 3. Rather than hand-assembling a stack in the caller, this
/// function lays out the exact frame the transition needs and hands back the
/// stack pointer to it.
///
/// # Safety
///
/// `stack_top` must be the top of a mapped user stack, and the program image
/// must already be loaded at the address it will run from.
pub unsafe fn user_context(entry: u64, stack_top: u64) -> u64 {
    // The `iretq` frame, laid out from the top down. Each `push` the CPU will
    // pop is 8 bytes, so the pushes below are in reverse order.
    //
    // `SS` and `RSP` are user selectors with RPL 3. `CS` likewise. `RFLAGS`
    // has bit 1 (reserved, must be 1) and bit 0 (clear — we are entering the
    // ring-0 side of the transition as the target of `iretq`).
    //
    // The selectors come from the HAL's GDT, so they cannot drift away from
    // the descriptors the kernel actually installed. The RPL is added here
    // because the GDT holds ring-0 descriptors and the CPU requires the
    // *loaded* selectors to name a descriptor whose DPL matches the
    // privilege level being entered.
    const SEL_K64_CODE: u64 = crate::gdt::KERNEL_CODE_SELECTOR as u64;
    const SEL_K64_DATA: u64 = crate::gdt::KERNEL_DATA_SELECTOR as u64;
    const SEL_U64_CODE: u64 = (crate::gdt::USER_CODE_SELECTOR as u64) | 0b11;
    const SEL_U64_DATA: u64 = (crate::gdt::USER_DATA_SELECTOR as u64) | 0b11;
    const RFLAGS_IF: u64 = 1 << 9;

    let mut sp = stack_top;
    macro_rules! push {
        ($value:expr) => {{
            sp -= 8;
            // SAFETY: `sp` is decremented before the write, and the caller
            // guarantees the whole stack region is mapped. Writing below the
            // top is what a stack does.
            unsafe {
                *(sp as *mut u64) = $value;
            }
        }};
    }

    // Pushed in reverse: the CPU pops in the order written.
    //
    // `iretq` to a lower privilege level pops **five** values: RIP, CS,
    // RFLAGS, RSP and SS. The frame therefore has to contain a fresh user
    // stack pointer as well as the user `SS`; omitting it makes the CPU read
    // `SS` as `RSP` and then fault fetching the real `SS` from one past the
    // top of the stack.
    push!(SEL_U64_DATA); // SS
                         // The program's initial stack pointer. Sixteen bytes below the top keeps
                         // the usual 16-byte alignment and leaves the top of the stack itself
                         // readable; the page *at* `stack_top` is deliberately unmapped.
    push!(stack_top - 16); // RSP
                           // The RFLAGS the program will run with. Bit 1 is reserved and must be 1;
                           // bit 0 is the reserved "carry" flag that the CPU forces to 0 on the way
                           // in, and the interrupt-enable flag is set so the program can be
                           // interrupted.
    push!(RFLAGS_IF | 0b10); // RFLAGS
    push!(SEL_U64_CODE); // CS
    push!(entry); // RIP
                  // The frame `iretq` expects, from low to high, is RIP, CS, RFLAGS, RSP, SS,
                  // which is the reverse of this push order, so the layout above is correct.
    let _ = SEL_K64_CODE;
    let _ = SEL_K64_DATA;
    sp
}

/// Install a GDT and TSS sufficient for ring 3.
///
/// # Why a TSS is required at all
///
/// `syscall`/`sysret` and `iretq` into ring 3 do not consult the TSS on entry,
/// but an interrupt taken *from* ring 3 loads `SS`/`RSP` from the TSS's
/// privilege stack. Without one, the first timer interrupt while a user
/// program runs faults. So the TSS is not optional once user mode exists, and
/// this is the point in the boot where it becomes necessary.
///
/// The GDT and the TSS live in the HAL's static storage: they must be valid
/// for the life of the kernel, and they must survive a page-table switch, so
/// heap allocation would be the wrong lifetime. The HAL owns them because the
/// GDT's *order* is constrained by the `syscall`/`sysret` selector arithmetic,
/// and that arithmetic has to agree with [`crate::gate::STAR_VALUE`] — so the
/// two must live in the same place to be able to agree at all.
///
/// # Safety
///
/// Must be called once, with interrupts disabled.
pub unsafe fn install_gdt() {
    // SAFETY: forwarded from this function's own contract: this runs once
    // during boot, with interrupts disabled, before any user code exists.
    unsafe { crate::gdt::install() };
}

/// Drop to ring 3 at `context`.
///
/// The context is a stack pointer to an `iretq` frame, as built by
/// [`user_context`].
///
/// # Safety
///
/// The caller must be running in ring 0 with interrupts disabled, and every
/// frame the `iretq` frame names must be mapped in the *current* address
/// space. A violation is a fault, not a return.
pub unsafe fn enter_user(context: u64) -> ! {
    // SAFETY: the caller guarantees the context is a valid `iretq` frame in the
    // current address space, and that its segment selectors are the ones
    // [`install_gdt`] published. The instruction does not return.
    unsafe {
        regs::enter_user_mode(context);
    }
}

/// Read the current privilege level, for diagnostics and tests.
///
/// 3 in user mode, 0 in the kernel.
pub fn current_cpl() -> u8 {
    if !regs::ON_BARE_METAL {
        return 0;
    }
    // SAFETY: reading CS does not change state.
    let cs = regs::read_cs();
    (cs & 0b11) as u8
}
