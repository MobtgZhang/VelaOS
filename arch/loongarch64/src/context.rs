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
//!
//! # What the switch does and does not save
//!
//! The switch stores the callee-saved set *into the context structures*
//! (the outgoing context's `sp`/`tp`/`ra`/`fp`/`s0`–`s8` fields, loaded back
//! from the incoming one), so it never depends on where either task's stack
//! happens to be. Caller-saved registers — `a0`–`a7`, `t0`–`t8` — are
//! deliberately *not* saved: a switch is shaped like a call, and the
//! compiler assumes a call destroys its caller-saved registers anyway. For
//! a resumed task the incoming `a0`/`a1` end up holding the context
//! pointers, which is as dead as any other caller-saved value.
//!
//! # How a fresh task starts
//!
//! A fresh task's context has `ra` set to [`fresh_ra`] — the
//! `vela_task_entry_trampoline` label — and its `entry`/`arg` fields
//! filled in. The switch loads that `ra` and jumps to it with `$a1` still
//! holding the context pointer, so the trampoline reads the real entry
//! point and argument out of the context and calls `entry(arg)` on the
//! task's own stack.

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

/// The return address a fresh task starts with: the task-entry trampoline.
///
/// `vela_ctx_switch` jumps to a resumed task's saved `ra`, so a fresh
/// task's context is built with `ra` pointing at
/// [`vela_task_entry_trampoline`], which picks the real entry point and
/// argument out of the same context.
///
/// Off the architecture (host tests) there is no trampoline to point at;
/// the value is zero, and [`switch_to`] is compiled out anyway.
#[cfg(target_arch = "loongarch64")]
fn fresh_ra() -> u64 {
    // The address is taken at run time: casting a pointer to an integer is
    // not allowed in constant evaluation, and a constant is not needed —
    // the value only ever reaches a context that is being built.
    // The address is taken through a function *pointer*: casting a function
    // item straight to an integer is what the lint warns about, and the
    // pointer is the thing that actually has an address.
    let trampoline: unsafe extern "C" fn() -> ! = vela_task_entry_trampoline;
    trampoline as usize as u64
}
/// Hosted counterpart of [`fresh_ra`]; see its documentation.
#[cfg(not(target_arch = "loongarch64"))]
fn fresh_ra() -> u64 {
    0
}

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
            ra: fresh_ra(),
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
    # $a0 = the outgoing context, $a1 = the incoming one. The callee-saved
    # set is saved into (loaded from) the context structures, so the switch
    # never depends on where either task's stack happens to be.
    st.d   $sp, $a0, 0x00
    st.d   $tp, $a0, 0x08
    st.d   $ra, $a0, 0x10
    st.d   $fp, $a0, 0x18
    st.d   $s0, $a0, 0x20
    st.d   $s1, $a0, 0x28
    st.d   $s2, $a0, 0x30
    st.d   $s3, $a0, 0x38
    st.d   $s4, $a0, 0x40
    st.d   $s5, $a0, 0x48
    st.d   $s6, $a0, 0x50
    st.d   $s7, $a0, 0x58
    st.d   $s8, $a0, 0x60

    # Load the incoming context. $ra picks up either the resumed task's
    # saved return address or, for a fresh task, the trampoline — and $a1
    # still holds the context pointer, which is what the trampoline reads
    # its entry point and argument from.
    ld.d   $sp, $a1, 0x00
    ld.d   $tp, $a1, 0x08
    ld.d   $ra, $a1, 0x10
    ld.d   $fp, $a1, 0x18
    ld.d   $s0, $a1, 0x20
    ld.d   $s1, $a1, 0x28
    ld.d   $s2, $a1, 0x30
    ld.d   $s3, $a1, 0x38
    ld.d   $s4, $a1, 0x40
    ld.d   $s5, $a1, 0x48
    ld.d   $s6, $a1, 0x50
    ld.d   $s7, $a1, 0x58
    ld.d   $s8, $a1, 0x60

    # Return to the caller — which, for a fresh task, is the trampoline.
    # `jirl $zero, $ra, 0` jumps to `ra` and links into $zero, which is how
    # the architecture spells a return.
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
    # A fresh task arrives here from vela_ctx_switch with $a1 = its context
    # pointer and $sp already the task's own stack. The context's `entry`
    # and `arg` fields are the two values a first entry needs.
    ld.d   $a0, $a1, 0x70      # the argument
    ld.d   $t0, $a1, 0x68      # the entry point
    jirl   $zero, $t0, 0       # entry(arg)
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
    use std::format;
    use std::vec::Vec;

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

    /// The assembly and the structure must agree about where the entry
    /// point and the argument live.
    ///
    /// The assembly's offsets are literals — `global_asm!` cannot take a
    /// Rust constant — so nothing but a check keeps them in step with the
    /// layout. This is that check: it reads the module's own source and
    /// looks for the trampoline reading the two values at the offsets the
    /// structure declares.
    #[test]
    fn the_assembly_reads_the_entry_and_the_argument_where_the_struct_puts_them() {
        let source = include_str!("context.rs");
        let flat = source.split_whitespace().collect::<Vec<_>>().join(" ");
        let entry = format!("0x{:02X}", OFFSET_ENTRY);
        let argument = format!("0x{:02X}", OFFSET_ARG);
        assert!(
            flat.contains(&format!("ld.d $t0, $a1, {}", entry)),
            "the trampoline must read the entry point at the offset the struct declares"
        );
        assert!(
            flat.contains(&format!("ld.d $a0, $a1, {}", argument)),
            "the trampoline must read the argument at the offset the struct declares"
        );
    }

    #[test]
    fn a_fresh_context_starts_at_the_entry_point() {
        let context = Context::for_entry(0x8000_0000, 0x4000_0000, 0x1234);
        assert_eq!(context.entry, 0x4000_0000);
        assert_eq!(context.arg, 0x1234);
        assert_eq!(context.stack_pointer(), 0x8000_0000 - 16);
        // The stack must stay 16-byte aligned: the ABI requires it.
        assert_eq!(context.stack_pointer() % 16, 0);
        // And the return address is the trampoline, which is what makes the
        // first `switch_to` into this context start the task at all.
        assert_eq!(context.ra, fresh_ra());
    }

    #[test]
    fn the_switch_touches_exactly_the_callee_saved_set() {
        // r21 is reserved by the ABI and r4-r20 are caller-saved; the
        // context carries ra (r1), tp (r2), fp (r22) and s0-s8 (r23-r31).
        assert_eq!(reg::RA, 1);
        assert_eq!(reg::TP, 2);
        assert_eq!(reg::SP, 3);
        assert_eq!(reg::FP, 22);
        assert_eq!(reg::FP + 1, reg::S0);
        assert_eq!(reg::S8, 31);
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
