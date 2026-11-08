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
//! The rule this module follows is: `swapgs` immediately on entry, and
//! `swapgs` immediately before `sysret`. Any code between those two points
//! may trust GS.
//!
//! # Mechanism vs policy
//!
//! This module owns the *mechanism*: the assembly entry stub, the MSR setup,
//! and the selector arithmetic. It does **not** own the *policy*: which
//! syscall number does what is decided by the kernel's dispatch table. The
//! stub therefore calls a kernel-provided dispatch function registered with
//! [`install`], keeping `vela-arch-x86_64` free of any kernel dependency.

use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use crate::regs;

/// The user stack pointer saved while a syscall is being handled.
///
/// # Why a static is safe here
///
/// `syscall` clears the interrupt-enable flag, so a handler cannot be
/// preempted by another syscall or by an interrupt, and v0.1.0 has one CPU.
/// The two conditions together mean only one syscall can be in flight at a
/// time, so one slot is enough. An SMP kernel would replace this with a
/// per-CPU value reached through `GS`.
static SYSCALL_USER_RSP: AtomicU64 = AtomicU64::new(0);

/// The kernel stack a syscall switches to on entry.
///
/// A syscall must not run on the user's stack: the program controls it, and a
/// handler that trusts it could be made to write wherever the program likes.
/// `start_init` installs the same ring-0 stack the TSS names for interrupts.
static SYSCALL_KERNEL_RSP: AtomicU64 = AtomicU64::new(0);

/// The address of the kernel's syscall dispatch function.
///
/// Set by [`install`] during boot. The assembly stub below calls through it,
/// so the kernel owns register reading and dispatch while this module owns the
/// entry mechanism.
static SYSCALL_DISPATCH: AtomicUsize = AtomicUsize::new(0);

/// Install the kernel stack a syscall switches to.
///
/// # Safety
///
/// `top` must be the top of a mapped, kernel-owned stack that stays valid for
/// the life of the machine, and this must be called before the first syscall.
pub fn set_kernel_stack(top: u64) {
    SYSCALL_KERNEL_RSP.store(top, Ordering::Release);
}

/// Install the `syscall` gate.
///
/// This must be called once, after the GDT is installed and before any user
/// code runs.
///
/// # Safety
///
/// Must be called with interrupts disabled, and only once per CPU. `dispatch`
/// is the kernel's syscall entry function: it reads the syscall number and
/// arguments from the registers, dispatches to the handler, and returns the
/// result. The stub calls it through the [`SYSCALL_DISPATCH`] pointer.
pub unsafe fn install(dispatch: unsafe extern "C" fn()) {
    // SAFETY: the caller guarantees this runs once, with interrupts off, and
    // that the GDT is installed so the selectors in `STAR_VALUE` are valid.
    unsafe {
        SYSCALL_DISPATCH.store(dispatch as usize, Ordering::Release);
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
pub const STAR_VALUE: u64 = (0x10u64 << 48) | (0x08u64 << 32);

/// The `IA32_SFMASK` value.
pub const SFMASK_VALUE: u64 = 1 << 9;

/// True when the `syscall` instruction is enabled.
pub fn is_enabled() -> bool {
    if !regs::ON_BARE_METAL {
        return false;
    }
    // SAFETY: `RDMSR` on `IA32_EFER` is always safe.
    let efer = regs::read_efer();
    efer & (1 << 0) != 0
}

// The assembly `syscall` entry stub.
//
// # Why this is assembly rather than a Rust function
//
// The CPU puts the user return address in `RCX` and the saved flags in
// `R11`, and `sysret` reads both back. A Rust function is free to use either
// register as scratch, so calling one directly would return to whatever
// happened to be in `RCX`. The registers are saved on the kernel stack before
// the call and restored after it, so the Rust handler can clobber them freely.
//
// The stub also switches off the user stack before doing anything that
// could push, and restores the user's stack pointer only after the handler
// has returned.
//
// The stub calls the kernel's dispatch function through the `SYSCALL_DISPATCH`
// pointer, so this module never depends on the kernel crate.
core::arch::global_asm!(
    r#"
    .text
    .global syscall_entry
    .type syscall_entry, @function
syscall_entry:
    swapgs
    mov qword ptr [rip + {user_rsp}], rsp
    mov rsp, qword ptr [rip + {kernel_rsp}]
    push rcx
    push r11
    call qword ptr [rip + {dispatch}]
    pop r11
    pop rcx
    mov rsp, qword ptr [rip + {user_rsp}]
    swapgs
    sysretq
    ud2
    .size syscall_entry, . - syscall_entry
"#,
    user_rsp = sym SYSCALL_USER_RSP,
    kernel_rsp = sym SYSCALL_KERNEL_RSP,
    dispatch = sym SYSCALL_DISPATCH,
);

extern "C" {
    /// The assembly stub above.
    fn syscall_entry();
}
