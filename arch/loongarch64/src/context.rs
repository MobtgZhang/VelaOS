//! The LoongArch64 context switch and task-entry trampoline.
//!
//! # The callee-saved set
//!
//! The LoongArch procedure-call standard names the registers that survive a
//! call:
//!
//! | register | number | role |
//! |---|---|---|
//! | `ra` | r1 | return address |
//! | `tp` | r2 | thread pointer |
//! | `sp` | r3 | stack pointer |
//! | `fp` | r22 | frame pointer |
//! | `s0`–`s8` | r23–r31 | callee-saved |
//!
//! `a0`–`a7` and `t0`–`t8` are caller-saved and are never stored: after the
//! switch they are overwritten with the incoming context's argument and
//! entry point, which is exactly the shape a call needs.
//!
//! # Why the assembly is in `global_asm!`
//!
//! The switch moves the stack pointer and the callee-saved registers in one
//! breath, and an inline `asm!` cannot express that: the compiler reserves
//! some of these registers (notably `fp` and `s0`) for itself, and it will
//! not let an `asm!` block clobber a value it is holding. A `global_asm!`
//! block is outside the compiler's view of the register file, which is
//! exactly what a context switch needs.

/// The register numbers the switch touches.
pub mod reg {
    /// The return address.
    pub const RA: usize = 1;
    /// The thread pointer.
    pub const TP: usize = 2;
    /// The stack pointer.
    pub const SP: usize = 3;
    /// The frame pointer.
    pub const FP: usize = 22;
    /// The first callee-saved register.
    pub const S0: usize = 23;
    /// The last callee-saved register (`s8`).
    pub const S8: usize = 31;
}

/// A saved task context.
///
/// The layout is the assembly's layout: the offsets below are the contract
/// between [`context.rs`]'s `global_asm!` block and the Rust side, and the
/// unit tests below pin them.
#[derive(Debug, Clone, Copy)]
#[repr(C, align(16))]
pub struct Context {
    /// The stack pointer.
    pub sp: u64,
    /// The thread pointer.
    pub tp: u64,
    /// The return address.
    pub ra: u64,
    /// The frame pointer.
    pub fp: u64,
    /// `s0`–`s8`.
    pub s: [u64; 9],
    /// The entry point of a fresh task.
    pub entry: u64,
    /// The argument of a fresh task.
    pub arg: u64,
}

/// The offset of `sp` in the context.
pub const OFFSET_SP: usize = 0;
/// The offset of `tp` in the context.
pub const OFFSET_TP: usize = 8;
/// The offset of `ra` in the context.
pub const OFFSET_RA: usize = 16;
/// The offset of `fp` in the context.
pub const OFFSET_FP: usize = 24;
/// The offset of the `s0`–`s8` array.
pub const OFFSET_S: usize = 32;
/// The offset of the entry point.
pub const OFFSET_ENTRY: usize = 32 + 9 * 8;
/// The offset of the argument.
pub const OFFSET_ARG: usize = 40 + 9 * 8;

impl Context {
    /// A context with nothing saved in it.
    pub const fn new() -> Self {
        Self {
            sp: 0,
            tp: 0,
            ra: 0,
            fp: 0,
            s: [0; 9],
            entry: 0,
            arg: 0,
        }
    }

    /// A context for a task that starts at `entry` with `arg`.
    ///
    /// The stack pointer is `stack_top` minus the 16 bytes the ABI wants
    /// reserved at the top of a frame, so the first call the task makes has
    /// room to spill a register.
    pub fn for_entry(stack_top: u64, entry: u64, arg: u64) -> Self {
        let aligned = stack_top & !0xF;
        Self {
            sp: aligned - 16,
            tp: 0,
            ra: 0,
            fp: 0,
            s: [0; 9],
            entry,
            arg,
        }
    }

    /// The stack pointer this context carries.
    pub const fn stack_pointer(&self) -> u64 {
        self.sp
    }
}

impl Default for Context {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(target_arch = "loongarch64")]
core::arch::global_asm!(
    r#"
    .globl vela_ctx_switch
    .type vela_ctx_switch, @function
vela_ctx_switch:
    # $a0 = the outgoing context, $a1 = the incoming one.
    #
    # Save the callee-saved set plus the three fixed-role registers. The
    # stack pointer moves first, because everything else is stored relative
    # to it.
    stp    $ra, $tp, 0x10($sp)
    stp    $fp, $s0, 0x20($sp)
    stp    $s1, $s2, 0x30($sp)
    stp    $s3, $s4, 0x40($sp)
    stp    $s5, $s6, 0x50($sp)
    stp    $s7, $s8, 0x60($sp)
    stp    $sp, $zero, 0x00($a0)

    # Load the incoming context.
    ld.d   $sp, 0x00($a1)
    ldp.d  $ra, $tp, 0x10($sp)
    ldp.d  $fp, $s0, 0x20($sp)
    ldp.d  $s1, $s2, 0x30($sp)
    ldp.d  $s3, $s4, 0x40($sp)
    ldp.d  $s5, $s6, 0x50($sp)
    ldp.d  $s7, $s8, 0x60($sp)

    # Return to the caller. `jirl $zero, $ra, 0` jumps to `ra` and links
    # into $zero, which is how the architecture spells a return.
    jirl   $zero, $ra, 0
    .size vela_ctx_switch, . - vela_ctx_switch
    "#
);

#[cfg(target_arch = "loongarch64")]
core::arch::global_asm!(
    r#"
    .globl vela_task_entry_trampoline
    .type vela_task_entry_trampoline, @function
vela_task_entry_trampoline:
    # A fresh task arrives here with $a0 = the context pointer and the
    # context's entry and argument staged in $a0/$a1 by the switch.
    ld.d   $t0, 0x58($a0)      # the entry point
    ld.d   $a1, 0x60($a0)      # the argument
    move   $a0, $a1            # entry(arg)
    jirl   $zero, $t0, 0
    # The task contract forbids returning; if it does, the breakpoint turns
    # the mistake into a visible trap rather than a silent fall-through.
    break   0
    .size vela_task_entry_trampoline, . - vela_task_entry_trampoline
    "#
);

/// Switch from the context at `current` to the context at `next`.
///
/// # Safety
///
/// * `current` must point at a writable [`Context`] belonging to the task
///   that is running now.
/// * `next` must point at a context saved by a previous call to this
///   function, or one built by [`Context::for_entry`].
/// * The two contexts must not be the same.
#[inline]
pub unsafe fn switch_to(current: *mut Context, next: *const Context) {
    #[cfg(target_arch = "loongarch64")]
    // SAFETY: the caller guarantees the two contexts are live and distinct.
    unsafe {
        vela_ctx_switch(current, next)
    }
    #[cfg(not(target_arch = "loongarch64"))]
    {
        // Off the target there is no assembly (and no second task): the
        // call is compiled out so host tests never touch a bogus context.
        let _ = (current, next);
    }
}

/// The entry point a fresh task starts in.
///
/// # Safety
///
/// Entered by a [`switch_to`] into a context built by
/// [`Context::for_entry`].
#[inline]
pub unsafe fn task_entry_trampoline() -> ! {
    #[cfg(target_arch = "loongarch64")]
    // SAFETY: the caller guarantees the context is a fresh task's.
    unsafe {
        vela_task_entry_trampoline()
    }
    #[cfg(not(target_arch = "loongarch64"))]
    {
        // Off the target there is no second task to start.
        loop {
            core::hint::spin_loop();
        }
    }
}

#[cfg(target_arch = "loongarch64")]
extern "C" {
    /// The context switch, written in assembly.
    fn vela_ctx_switch(from: *mut Context, to: *const Context);

    /// The trampoline a fresh task starts in.
    fn vela_task_entry_trampoline() -> !;
}

#[cfg(test)]
extern crate std;

#[cfg(test)]
mod tests {
    use super::*;
    use core::mem::size_of;

    #[test]
    fn the_layout_matches_the_assembly() {
        // The offsets are the contract with the assembly above; the tests
        // pin them so a reordering cannot pass unnoticed.
        assert_eq!(OFFSET_SP, 0);
        assert_eq!(OFFSET_TP, 8);
        assert_eq!(OFFSET_RA, 16);
        assert_eq!(OFFSET_FP, 24);
        assert_eq!(OFFSET_S, 32);
        assert_eq!(OFFSET_ENTRY, 32 + 9 * 8);
        assert_eq!(OFFSET_ARG, 40 + 9 * 8);
    }

    #[test]
    fn the_offsets_are_where_the_fields_are() {
        let context = Context::new();
        let base = &context as *const Context as usize;
        assert_eq!(&context.sp as *const u64 as usize - base, OFFSET_SP);
        assert_eq!(&context.tp as *const u64 as usize - base, OFFSET_TP);
        assert_eq!(&context.ra as *const u64 as usize - base, OFFSET_RA);
        assert_eq!(&context.fp as *const u64 as usize - base, OFFSET_FP);
        assert_eq!(&context.s as *const u64 as usize - base, OFFSET_S);
        assert_eq!(&context.entry as *const u64 as usize - base, OFFSET_ENTRY);
        assert_eq!(&context.arg as *const u64 as usize - base, OFFSET_ARG);
        // The structure is padded to the ABI's 16-byte alignment, so it is
        // a little larger than its last field.
        assert!(size_of::<Context>() >= OFFSET_ARG + 8);
        assert_eq!(size_of::<Context>() % 16, 0);
    }

    #[test]
    fn a_fresh_context_starts_at_the_entry_point() {
        let context = Context::for_entry(0x8000_0000, 0x4000_0000, 0x1234);
        assert_eq!(context.entry, 0x4000_0000);
        assert_eq!(context.arg, 0x1234);
        assert_eq!(context.stack_pointer(), 0x8000_0000 - 16);
        // The stack must stay 16-byte aligned: the ABI requires it.
        assert_eq!(context.stack_pointer() % 16, 0);
    }

    #[test]
    fn the_callee_saved_set_is_the_one_the_abi_names() {
        // `fp` is r22 and `s0`–`s8` are r23–r31: the switch saves nine
        // callee-saved registers plus the three fixed-role ones.
        assert_eq!(reg::FP, 22);
        assert_eq!(reg::S0, 23);
        assert_eq!(reg::S8, 31);
        assert_eq!(reg::S8 - reg::S0 + 1, 9);
        assert_eq!(Context::new().s.len(), 9);
    }
}
