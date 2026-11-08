//! The x86-64 context switch and task-entry trampoline.
//!
//! # Why this lives in the architecture crate
//!
//! A context switch is not scheduler policy: it is a register save/restore
//! contract that depends on the CPU's ABI and its stack layout. The scheduler
//! selects *which* task runs; this module performs the *transfer* between the
//! two tasks. Keeping the assembly here means `kernel/scheduler` contains no
//! raw assembly and remains implementable for the other architectures.
//!
//! # What `switch_to` does
//!
//! It saves the outgoing task's callee-saved registers into `current`, loads
//! the incoming task's from `next`, and returns into the incoming task's stack.
//! The save and the load are symmetric.
//!
//! The incoming task's first entry is reached through the trampoline rather
//! than a restored `RIP`, because a new task has no saved return address. The
//! trampoline calls the entry point with the argument in `rdi`, as the System V
//! ABI requires.
//!
//! # Why the assembly is in `global_asm!`
//!
//! LLVM reserves `rbx` for its own use and refuses to name it as an `asm!`
//! operand, so an inline block cannot express "save `rbx` here". `global_asm!`
//! is not an operand of a Rust function, so LLVM never reasons about the
//! registers it touches.
//!
//! # The `xmm` registers
//!
//! `xmm6`–`xmm15` are callee-saved under the System V ABI. The kernel does not
//! use floating point, but a user task may, so they are saved and restored too.

use core::arch::global_asm;

/// The saved register context of a task.
///
/// The layout is part of the assembly contract: `switch_to` addresses the
/// fields by these exact offsets. The `Context` struct is the single source of
/// truth, and the offset tests below catch drift between the two.
#[derive(Debug, Clone, Copy)]
#[repr(C, align(16))]
pub struct Context {
    /// The stack pointer.
    pub rsp: u64,
    /// The base pointer.
    pub rbp: u64,
    /// The callee-saved general registers.
    pub rbx: u64,
    /// `r12`–`r15`, also callee-saved.
    pub r12: u64,
    /// `r13`, callee-saved.
    pub r13: u64,
    /// `r14`, callee-saved.
    pub r14: u64,
    /// `r15`, callee-saved.
    pub r15: u64,
    /// The callee-saved XMM registers.
    pub xmm: [u128; 10],
}

impl Default for Context {
    fn default() -> Self {
        Self::new()
    }
}

impl Context {
    /// A zeroed context.
    ///
    /// Used for a task that has not run yet: its first entry goes through the
    /// trampoline, which sets the registers a fresh task needs.
    pub const fn new() -> Self {
        Self {
            rsp: 0,
            rbp: 0,
            rbx: 0,
            r12: 0,
            r13: 0,
            r14: 0,
            r15: 0,
            xmm: [0; 10],
        }
    }

    /// The context of a task that starts by calling `entry(arg)`.
    ///
    /// `stack_top` must be the top of the task's kernel stack; the stack grows
    /// down, so the first push lands just below it. The alignment is fixed so
    /// the ABI's 16-byte rule holds at the first `call`.
    pub fn for_entry(stack_top: u64, entry: u64, arg: u64) -> Self {
        let aligned = stack_top & !0xF;
        Self {
            rsp: aligned - 16,
            rbp: 0,
            rbx: 0,
            r12: entry,
            r13: arg,
            r14: 0,
            r15: 0,
            xmm: [0; 10],
        }
    }

    /// The stack pointer this context will resume on.
    pub const fn stack_pointer(&self) -> u64 {
        self.rsp
    }
}

// The context switch itself.
//
// `rdi` is the outgoing context and `rsi` the incoming one, matching the
// System V ABI's first two integer arguments.
//
// The XMM block starts at offset 0x40, leaving a 16-byte hole after `r15` so
// that every `movdqu` access is 16-byte aligned.
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
// `Context::for_entry`. The entry point is called with the argument in `rdi`.
global_asm!(
    ".globl vela_task_entry_trampoline",
    ".type vela_task_entry_trampoline, @function",
    "vela_task_entry_trampoline:",
    "mov rdi, r13",
    "mov rax, r12",
    "and rsp, -16",
    "call rax",
    "ud2",
    ".size vela_task_entry_trampoline, . - vela_task_entry_trampoline",
);

extern "C" {
    /// The context switch, written in assembly.
    fn vela_switch_to(from: *mut Context, to: *const Context);

    /// The trampoline a new task starts in.
    fn vela_task_entry_trampoline() -> !;
}

/// Switch from the context at `current` to the context at `next`.
///
/// # Safety
///
/// * `current` must point at a writable `Context` belonging to the task that
///   is running now.
/// * `next` must point at a context saved by a previous call to this function,
///   or one built by [`Context::for_entry`].
/// * The two contexts must not be the same.
#[inline]
pub unsafe fn switch_to(current: *mut Context, next: *const Context) {
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
    unsafe { vela_task_entry_trampoline() }
}

extern crate alloc;

#[cfg(test)]
extern crate std;

#[cfg(test)]
mod tests {
    use super::*;
    use core::mem::{align_of, offset_of, size_of};

    #[test]
    fn the_assembly_offsets_match_the_struct_layout() {
        assert_eq!(offset_of!(Context, rsp), 0x00);
        assert_eq!(offset_of!(Context, rbp), 0x08);
        assert_eq!(offset_of!(Context, rbx), 0x10);
        assert_eq!(offset_of!(Context, r12), 0x18);
        assert_eq!(offset_of!(Context, r13), 0x20);
        assert_eq!(offset_of!(Context, r14), 0x28);
        assert_eq!(offset_of!(Context, r15), 0x30);
        assert_eq!(offset_of!(Context, xmm), 0x40);
    }

    #[test]
    fn the_xmm_block_is_aligned_for_movdqu() {
        let base = offset_of!(Context, xmm);
        assert_eq!(base % 16, 0);
        assert_eq!(base + 10 * 16, size_of::<Context>());
    }

    #[test]
    fn the_context_is_sixteen_byte_aligned() {
        assert_eq!(align_of::<Context>(), 16);
    }

    #[test]
    fn a_fresh_context_is_zeroed() {
        let c = Context::new();
        assert_eq!(c.rsp, 0);
        assert_eq!(c.xmm, [0u128; 10]);
    }

    #[test]
    fn an_entry_context_carries_the_entry_and_the_argument() {
        let c = Context::for_entry(0x8000, 0xDEAD_BEEF, 0x1234);
        assert_eq!(c.r12, 0xDEAD_BEEF);
        assert_eq!(c.r13, 0x1234);
        assert!(c.rsp < 0x8000);
        assert_eq!(c.rsp & 0xF, 0);
    }

    #[test]
    fn the_trampoline_and_switch_are_distinct_symbols() {
        let switch = vela_switch_to as *const () as usize;
        let trampoline = vela_task_entry_trampoline as *const () as usize;
        assert_ne!(switch, trampoline);
    }
}
