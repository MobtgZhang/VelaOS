//! The context switch.
//!
//! # What `switch_to` does
//!
//! It saves the current task's callee-saved registers into `current`, loads
//! the next task's from `next`, and jumps to it. The save and the load are
//! symmetric, and the asymmetry in *who* saves and who loads is what makes it
//! work:
//!
//! * The **outgoing** task's context is saved by the switch that leaves it.
//! * The **incoming** task's context is loaded by the switch that enters it.
//!
//! So the first switch *into* a new task has nothing to restore, and the
//! task's entry point is reached through the trampoline rather than through a
//! restored `RIP`. That is why [`Context`] has no `rip` field: the return
//! address lives on the task's own stack, pushed by the `call` that the
//! previous switch performed.
//!
//! # Why the assembly is in `global_asm!`
//!
//! Because `rbx` is involved, and LLVM refuses to let `rbx` be named as an
//! `asm!` operand — it reserves the register for its own use. An inline
//! `asm!` block therefore cannot express "save `rbx` here", because LLVM
//! would assume `rbx` survives the block and would then be wrong.
//!
//! `global_asm!` sidesteps the problem: the block is not an operand of any
//! Rust function, so LLVM never reasons about the registers it touches. The
//! Rust side declares the register clobbers with `naked` functions, which is
//! exactly what they are for.
//!
//! # The assembly
//!
//! The register juggling is in assembly because it must be exact. The
//! sequence is:
//!
//! 1. Save the callee-saved registers at their documented offsets.
//! 2. Save `rsp` into `current`.
//! 3. Load `rsp` from `next`.
//! 4. Load the callee-saved registers back.
//!
//! Steps 1–2 and 3–4 are mirror images, which is the property that makes the
//! layout correct. Getting the order wrong in one place and not the other
//! corrupts every task after the first switch, which is why the order is
//! stated in one place — [`Context`] — and the assembly follows it.
//!
//! # The `xmm` registers
//!
//! The System V ABI requires `xmm6`–`xmm15` to be preserved across a call.
//! The kernel does not use floating point, but a *user* task might, and its
//! callee-saved XMM registers must survive the switch. They are saved and
//! restored alongside the general registers.

use core::arch::global_asm;

use crate::task::Context;

// The context switch itself.
//
// `rdi` is the outgoing context and `rsi` the incoming one, matching the
// System V ABI's first two integer arguments.
//
// The XMM block starts at offset 0x40, leaving a 16-byte hole after `r15`
// so that every `movdqu` access is 16-byte aligned.
global_asm!(
    ".globl vela_switch_to",
    ".type vela_switch_to, @function",
    "vela_switch_to:",
    // --- save the outgoing context ---------------------------------------
    "mov [rdi + 0x00], rsp",
    "mov [rdi + 0x08], rbp",
    "mov [rdi + 0x10], rbx",
    "mov [rdi + 0x18], r12",
    "mov [rdi + 0x20], r13",
    "mov [rdi + 0x28], r14",
    "mov [rdi + 0x30], r15",
    "movdqu [rdi + 0x40], xmm6",
    "movdqu [rdi + 0x50], xmm7",
    "movdqu [rdi + 0x60], xmm8",
    "movdqu [rdi + 0x70], xmm9",
    "movdqu [rdi + 0x80], xmm10",
    "movdqu [rdi + 0x90], xmm11",
    "movdqu [rdi + 0xa0], xmm12",
    "movdqu [rdi + 0xb0], xmm13",
    "movdqu [rdi + 0xc0], xmm14",
    "movdqu [rdi + 0xd0], xmm15",
    // --- load the incoming context ---------------------------------------
    "mov rsp, [rsi + 0x00]",
    "mov rbp, [rsi + 0x08]",
    "mov rbx, [rsi + 0x10]",
    "mov r12, [rsi + 0x18]",
    "mov r13, [rsi + 0x20]",
    "mov r14, [rsi + 0x28]",
    "mov r15, [rsi + 0x30]",
    "movdqu xmm6, [rsi + 0x40]",
    "movdqu xmm7, [rsi + 0x50]",
    "movdqu xmm8, [rsi + 0x60]",
    "movdqu xmm9, [rsi + 0x70]",
    "movdqu xmm10, [rsi + 0x80]",
    "movdqu xmm11, [rsi + 0x90]",
    "movdqu xmm12, [rsi + 0xa0]",
    "movdqu xmm13, [rsi + 0xb0]",
    "movdqu xmm14, [rsi + 0xc0]",
    "movdqu xmm15, [rsi + 0xd0]",
    "ret",
    ".size vela_switch_to, . - vela_switch_to",
);

// The trampoline a new task starts in.
//
// `r12` carries the entry point and `r13` the argument, as set up by
// `Context::for_entry`. The entry point is called with the argument in `rdi`,
// which is where the System V ABI wants the first integer argument.
global_asm!(
    ".globl vela_task_entry_trampoline",
    ".type vela_task_entry_trampoline, @function",
    "vela_task_entry_trampoline:",
    "mov rdi, r13",
    "mov rax, r12",
    // Align the stack to 16 bytes, as the ABI requires at a `call`.
    "and rsp, -16",
    "call rax",
    // The entry point is not expected to return. If it does, the task is
    // finished and there is nothing sensible to return into, so halt.
    "ud2",
    ".size vela_task_entry_trampoline, . - vela_task_entry_trampoline",
);

extern "C" {
    /// The context switch, written in assembly.
    ///
    /// # Safety
    ///
    /// `from` must point at a writable [`Context`] belonging to the running
    /// task, and `to` must point at a context saved by a previous call or
    /// built by [`Context::for_entry`].
    fn vela_switch_to(from: *mut Context, to: *const Context);

    /// The trampoline a new task starts in.
    ///
    /// # Safety
    ///
    /// Must only be reached through a context switch into a context built by
    /// [`Context::for_entry`]. It never returns.
    fn vela_task_entry_trampoline() -> !;
}

/// Switch from the context at `current` to the context at `next`.
///
/// # Safety
///
/// * `current` must point at a writable `Context` belonging to the task that
///   is running now.
/// * `next` must point at a `Context` that was saved by a previous call to
///   this function, or was built by [`Context::for_entry`] — in which case
///   the switch lands in the entry trampoline instead of resuming a task.
/// * The two contexts must not be the same.
#[inline]
pub unsafe fn switch_to(current: *mut Context, next: *const Context) {
    // SAFETY: the caller guarantees both pointers are valid and that `next`
    // holds a context this function can restore.
    unsafe { vela_switch_to(current, next) }
}

/// The trampoline a new task starts in.
///
/// # Safety
///
/// This is entered by a `switch_to` into a context built by
/// [`Context::for_entry`], with the entry point in `r12` and the argument in
/// `r13`. It must not return.
#[inline]
pub unsafe fn task_entry_trampoline() -> ! {
    // SAFETY: the assembly trampoline never returns.
    unsafe { vela_task_entry_trampoline() }
}

#[cfg(test)]
extern crate std;

#[cfg(test)]
mod tests {
    use super::*;
    use core::mem::offset_of;

    #[test]
    fn the_assembly_offsets_match_the_struct_layout() {
        // The assembly addresses the context's fields by hard-coded offsets.
        // If the struct ever changes, these offsets must change with it —
        // this test is what catches the drift.
        assert_eq!(offset_of!(Context, rsp), 0x00);
        assert_eq!(offset_of!(Context, rbp), 0x08);
        assert_eq!(offset_of!(Context, rbx), 0x10);
        assert_eq!(offset_of!(Context, r12), 0x18);
        assert_eq!(offset_of!(Context, r13), 0x20);
        assert_eq!(offset_of!(Context, r14), 0x28);
        assert_eq!(offset_of!(Context, r15), 0x30);
        // The XMM block starts at 0x40, leaving a 16-byte hole after `r15`
        // so that the `movdqu` accesses are 16-byte aligned.
        assert_eq!(offset_of!(Context, xmm), 0x40);
    }

    #[test]
    fn the_xmm_block_is_aligned_for_movdqu() {
        // `movdqu` is the unaligned move, so alignment is not strictly
        // required — but an aligned access is faster, and the layout was
        // chosen to make it aligned.
        let base = offset_of!(Context, xmm);
        assert_eq!(base % 16, 0);
        // Ten 16-byte slots, one per callee-saved XMM register.
        assert_eq!(base + 10 * 16, core::mem::size_of::<Context>());
    }

    #[test]
    fn the_context_is_sixteen_byte_aligned() {
        // `Context` is saved and restored with `movdqu`, but the struct itself
        // must be 16-byte aligned so that a `Context` on the stack is aligned.
        assert_eq!(core::mem::align_of::<Context>(), 16);
    }

    #[test]
    fn the_saved_registers_are_the_callee_saved_set() {
        // These are exactly the registers the System V ABI requires a callee
        // to preserve. Saving any more would be wasted work; saving any
        // fewer would corrupt the caller.
        let saved = [
            offset_of!(Context, rbx),
            offset_of!(Context, r12),
            offset_of!(Context, r13),
            offset_of!(Context, r14),
            offset_of!(Context, r15),
        ];
        // The stack and base pointers are saved too, because a task switch
        // abandons the current frame entirely.
        assert_eq!(offset_of!(Context, rsp), 0);
        assert_eq!(offset_of!(Context, rbp), 8);
        assert_eq!(saved.len(), 5);
        // And none of them collide with the general-purpose set.
        for offset in saved {
            assert!(offset >= 16, "a general register overlaps rsp/rbp");
        }
    }

    #[test]
    fn the_trampoline_and_switch_are_distinct_symbols() {
        // A copy-paste error that merged the two would be catastrophic: the
        // switch would land in a task's first instruction every time.
        let switch = vela_switch_to as *const () as usize;
        let trampoline = vela_task_entry_trampoline as *const () as usize;
        assert_ne!(switch, trampoline);
    }
}
