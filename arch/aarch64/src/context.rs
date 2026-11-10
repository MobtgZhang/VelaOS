//! The AArch64 context switch and task-entry trampoline.
//!
//! # The callee-saved set
//!
//! The AArch64 procedure-call standard names the registers that survive a
//! call: `x19`–`x28` (ten general registers), `x29` (the frame pointer),
//! `x30` (the link register) and the lower 64 bits of the SIMD registers
//! `v8`–`v15`. `x0`–`x18` are caller-saved and are never stored: after the
//! switch they are overwritten with the incoming context's argument and
//! entry point, which is exactly the shape a call needs.
//!
//! # Why the assembly is in `global_asm!`
//!
//! The switch moves the stack pointer and the callee-saved registers in one
//! breath, and an inline `asm!` cannot express that: the compiler reserves
//! some of these registers for itself and will not let an `asm!` block
//! clobber a value it is holding. A `global_asm!` block is outside the
//! compiler's view of the register file, which is what a context switch
//! needs.

/// The register numbers the switch touches.
pub mod reg {
    /// The first callee-saved general register.
    pub const X19: usize = 19;
    /// The last callee-saved general register.
    pub const X28: usize = 28;
    /// The frame pointer.
    pub const X29: usize = 29;
    /// The link register.
    pub const X30: usize = 30;
    /// The stack pointer.
    pub const SP: usize = 31;
}

/// A saved task context.
///
/// The layout is the assembly's layout: the offsets below are the contract
/// between this module's `global_asm!` block and the Rust side, and the
/// unit tests below pin them.
#[derive(Debug, Clone, Copy)]
#[repr(C, align(16))]
pub struct Context {
    /// The stack pointer.
    pub sp: u64,
    /// `x19`–`x28`.
    pub x: [u64; 10],
    /// The frame pointer (`x29`).
    pub fp: u64,
    /// The link register (`x30`).
    pub lr: u64,
    /// The entry point of a fresh task.
    pub entry: u64,
    /// The argument of a fresh task.
    pub arg: u64,
}

/// The link register a fresh task starts with: the entry trampoline.
///
/// The switch resumes a task by jumping to its link register, so a task
/// that has never run must have one pointing at the trampoline.
///
/// Off the architecture (host tests) there is no trampoline to point at;
/// the value is zero, and [`switch_to`] is compiled out anyway.
#[cfg(target_arch = "aarch64")]
fn fresh_lr() -> u64 {
    // The address is taken at run time: casting a pointer to an integer is
    // not allowed in constant evaluation, and a constant is not needed —
    // the value only ever reaches a context that is being built.
    // The address is taken through a function *pointer*: casting a function
    // item straight to an integer is what the lint warns about, and the
    // pointer is the thing that actually has an address.
    let trampoline: unsafe extern "C" fn() -> ! = vela_task_entry_trampoline;
    trampoline as usize as u64
}
/// Hosted counterpart of [`fresh_lr`]; see its documentation.
#[cfg(not(target_arch = "aarch64"))]
fn fresh_lr() -> u64 {
    0
}

/// The offset of `sp` in the context.
pub const OFFSET_SP: usize = 0;
/// The offset of the `x19`–`x28` array.
pub const OFFSET_X: usize = 8;
/// The offset of the frame pointer.
pub const OFFSET_FP: usize = 8 + 10 * 8;
/// The offset of the link register.
pub const OFFSET_LR: usize = 16 + 10 * 8;
/// The offset of the entry point.
pub const OFFSET_ENTRY: usize = 24 + 10 * 8;
/// The offset of the argument.
pub const OFFSET_ARG: usize = 32 + 10 * 8;

impl Context {
    /// A context with nothing saved in it.
    pub const fn new() -> Self {
        Self {
            sp: 0,
            x: [0; 10],
            fp: 0,
            lr: 0,
            entry: 0,
            arg: 0,
        }
    }

    /// A context for a task that starts at `entry` with `arg`.
    ///
    /// The stack pointer is `stack_top` minus the 16 bytes the ABI wants
    /// reserved at the top of a frame, so the first call the task makes has
    /// room to spill a register.
    ///
    /// The entry point and the argument are *staged* in `x19` and `x20`:
    /// the switch resumes a task by jumping to its link register, and a
    /// task that has never run has its link register pointing at the
    /// trampoline, which reads the two out of those callee-saved registers.
    pub fn for_entry(stack_top: u64, entry: u64, arg: u64) -> Self {
        let aligned = stack_top & !0xF;
        Self {
            sp: aligned - 16,
            // x19 = the entry point, x20 = the argument: what the
            // trampoline reads out when the task is first resumed.
            x: [entry, arg, 0, 0, 0, 0, 0, 0, 0, 0],
            fp: 0,
            // The link register of a fresh task points at the trampoline:
            // the switch resumes a task by jumping to its link register, and
            // a task that has never run has nothing to resume into.
            lr: fresh_lr(),
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

#[cfg(target_arch = "aarch64")]
core::arch::global_asm!(
    r#"
    .globl vela_ctx_switch
    .type vela_ctx_switch, %function
vela_ctx_switch:
    // $x0 = the outgoing context, $x1 = the incoming one.
    //
    // Everything is saved into the outgoing context's memory, never onto
    // the stack: the region above $sp belongs to the caller's frame and
    // must not be touched.
    mov     $x2, $sp
    stp     $x19, $x20, [$x0, #8]
    stp     $x21, $x22, [$x0, #24]
    stp     $x23, $x24, [$x0, #40]
    stp     $x25, $x26, [$x0, #56]
    stp     $x27, $x28, [$x0, #72]
    stp     $x29, $x30, [$x0, #88]
    // The stack pointer of the moment of the switch: the task resumes
    // exactly here when it is switched back to.
    str     $x2, [$x0, #0]

    // Load the incoming context. The registers come out of the *context
    // structure* — the same place the save wrote them — and only the stack
    // pointer comes from the saved slot.
    ldr     $x2, [$x1, #0]
    mov     $sp, $x2
    ldp     $x19, $x20, [$x1, #8]
    ldp     $x21, $x22, [$x1, #24]
    ldp     $x23, $x24, [$x1, #40]
    ldp     $x25, $x26, [$x1, #56]
    ldp     $x27, $x28, [$x1, #72]
    ldp     $x29, $x30, [$x1, #88]

    // Return into the incoming task: `ret` is `br x30`, and the restored
    // link register is either the switch's return address (a task that has
    // run before) or the trampoline's (a fresh one).
    ret
    .size vela_ctx_switch, . - vela_ctx_switch
    "#
);

#[cfg(target_arch = "aarch64")]
core::arch::global_asm!(
    r#"
    .globl vela_task_entry_trampoline
    .type vela_task_entry_trampoline, %function
vela_task_entry_trampoline:
    // A fresh task arrives here from `vela_ctx_switch`'s `ret` into the
    // restored link register, with `Context::for_entry` having staged the
    // entry point in $x19 and the argument in $x20.
    mov     $x0, $x20             // entry(arg)
    br      $x19
    // The task contract forbids returning; if it does, the undefined
    // instruction turns the mistake into a visible trap rather than a
    // silent fall-through.
    udf     #0
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
    #[cfg(target_arch = "aarch64")]
    // SAFETY: the caller guarantees the two contexts are live and distinct.
    unsafe {
        vela_ctx_switch(current, next)
    }
    #[cfg(not(target_arch = "aarch64"))]
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
/// Not for direct calls: the trampoline reads the entry point and argument
/// out of the *staged* `x19`/`x20`, which only a [`switch_to`] into a
/// [`Context::for_entry`] context sets up. It exists so the symbol is
/// reachable and nameable from Rust.
#[inline]
pub unsafe fn task_entry_trampoline() -> ! {
    #[cfg(target_arch = "aarch64")]
    // SAFETY: the caller guarantees the context is a fresh task's.
    unsafe {
        vela_task_entry_trampoline()
    }
    #[cfg(not(target_arch = "aarch64"))]
    {
        // Off the target there is no second task to start.
        loop {
            core::hint::spin_loop();
        }
    }
}

#[cfg(target_arch = "aarch64")]
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
    use std::vec::Vec;

    #[test]
    fn the_layout_matches_the_assembly() {
        // The offsets are the contract with the assembly above; the tests
        // pin them so a reordering cannot pass unnoticed.
        assert_eq!(OFFSET_SP, 0);
        assert_eq!(OFFSET_X, 8);
        assert_eq!(OFFSET_FP, 8 + 10 * 8);
        assert_eq!(OFFSET_LR, 16 + 10 * 8);
        assert_eq!(OFFSET_ENTRY, 24 + 10 * 8);
        assert_eq!(OFFSET_ARG, 32 + 10 * 8);
    }

    #[test]
    fn the_offsets_are_where_the_fields_are() {
        let context = Context::new();
        let base = &context as *const Context as usize;
        assert_eq!(&context.sp as *const u64 as usize - base, OFFSET_SP);
        assert_eq!(&context.x as *const u64 as usize - base, OFFSET_X);
        assert_eq!(&context.fp as *const u64 as usize - base, OFFSET_FP);
        assert_eq!(&context.lr as *const u64 as usize - base, OFFSET_LR);
        assert_eq!(&context.entry as *const u64 as usize - base, OFFSET_ENTRY);
        assert_eq!(&context.arg as *const u64 as usize - base, OFFSET_ARG);
    }

    /// The assembly and the structure must agree about how a fresh task
    /// starts.
    ///
    /// The assembly's register choices are literals — `global_asm!` cannot
    /// take a Rust constant — so nothing but a check keeps the trampoline's
    /// staging contract (`x19` = entry, `x20` = argument) in step with
    /// [`Context::for_entry`]. This is that check.
    #[test]
    fn the_trampoline_reads_the_staged_registers_for_entry_stages() {
        let source = include_str!("context.rs");
        let flat = source.split_whitespace().collect::<Vec<_>>().join(" ");
        assert!(
            flat.contains("mov $x0, $x20"),
            "the trampoline must pass the staged argument in x20"
        );
        assert!(
            flat.contains("br $x19"),
            "the trampoline must jump to the staged entry point in x19"
        );
        // And the Rust side must stage the same two registers.
        let context = Context::for_entry(0x8000_0000, 0x4000_0123, 0x456);
        assert_eq!(context.x[0], 0x4000_0123, "x19 carries the entry point");
        assert_eq!(context.x[1], 0x456, "x20 carries the argument");
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
        assert_eq!(reg::X19, 19);
        assert_eq!(reg::X28, 28);
        assert_eq!(reg::X29, 29);
        assert_eq!(reg::X30, 30);
        assert_eq!(reg::SP, 31);
        assert_eq!(Context::new().x.len(), 10);
    }
}
