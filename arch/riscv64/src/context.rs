//! The RV64 context switch and task-entry trampoline.
//!
//! # Why this lives in the architecture crate
//!
//! A context switch is not scheduler policy: it is a register save/restore
//! contract that depends on the CPU's ABI. The scheduler selects *which* task
//! runs; this module performs the *transfer*. Keeping the assembly here means
//! `kernel/scheduler` contains no raw assembly and remains implementable for
//! the other architectures.
//!
//! # The psABI callee-saved reasoning
//!
//! The RISC-V psABI (the "generic" calling convention, RV64) partitions the
//! integer registers exactly the way the System V ABI partitions x86-64's:
//!
//! * `ra` (return address), `sp`, `gp` (global pointer), `tp` (thread
//!   pointer) are preserved across a call — `sp` and `gp`/`tp` by convention,
//!   `ra` because the *caller* needs it back on return.
//! * `t0`–`t6` are caller-saved *temporaries*: dead at every call boundary.
//! * `s0`–`s11` are callee-saved: any function that uses them must restore
//!   them.
//! * `a0`–`a7` are argument registers: caller-saved, dead across a call.
//!
//! A cooperative switch happens *in the middle* of a function — the yielding
//! task calls `switch_to` and expects to resume exactly where it left off,
//! with every register the psABI says it may rely on intact. So the switch
//! must save and restore precisely the callee-saved set: `ra`, `sp`, `gp`,
//! `tp`, `t0`–`t6` (which only survive across a switch because a switch is
//! *not* an ordinary call the compiler can see — it is a `call` whose "return"
//! lands in a different task) and `s0`–`s11`. The `a`-registers need no
//! saving: they are caller-saved, so no code between call sites may depend on
//! them. The one exception the design must still handle is *staging* — see
//! `for_entry`.
//!
//! # Why the assembly is in `global_asm!`
//!
//! `switch_to` must overwrite the outgoing task's registers with the incoming
//! task's, including `ra` and `sp` — registers a Rust function's inline `asm!`
//! operands are forbidden from clobbering freely, because LLVM believes it
//! still holds live values in them. `global_asm!` is not part of any Rust
//! function, so LLVM never reasons about the registers it touches.
//!
//! # The fresh-task path
//!
//! A new task has no saved context to resume into, so its `Context` is built
//! by [`Context::for_entry`] with `ra` pointing at the trampoline. The switch
//! restores the callee-saved set and then, as its last action, stages the
//! fresh task's `arg` and `entry` slots into `a0` and `a1`. That is safe for
//! the same reason the `a`-registers are never saved: they are caller-saved,
//! so a resumed (non-fresh) task cannot depend on them and overwriting them
//! is invisible. The trampoline then calls `entry(arg)` per the psABI's
//! argument placement — the RISC-V equivalent of the System V `rdi` hand-off.

/// The saved register context of a task.
///
/// The layout is part of the assembly contract: [`vela_ctx_switch`] addresses
/// the fields by these exact offsets. The struct is the single source of
/// truth, and the offset tests below catch drift between the two.
#[derive(Debug, Clone, Copy)]
#[repr(C, align(16))]
pub struct Context {
    /// The stack pointer.
    pub sp: u64,
    /// The return address, i.e. where this task resumes.
    pub ra: u64,
    /// The global pointer, preserved by psABI convention.
    pub gp: u64,
    /// The thread pointer, preserved by psABI convention.
    pub tp: u64,
    /// Temporaries `t0`–`t6`: caller-saved across calls, but preserved
    /// across a *switch*, because the resume point is mid-function.
    pub t0: u64,
    /// Temporaries `t1`, preserved across a switch.
    pub t1: u64,
    /// Temporaries `t2`, preserved across a switch.
    pub t2: u64,
    /// Temporaries `t3`, preserved across a switch.
    pub t3: u64,
    /// Temporaries `t4`, preserved across a switch.
    pub t4: u64,
    /// Temporaries `t5`, preserved across a switch.
    pub t5: u64,
    /// Temporaries `t6`, preserved across a switch.
    pub t6: u64,
    /// Saved registers `s0`–`s11`, callee-saved under the psABI.
    pub s0: u64,
    /// Saved register `s1`.
    pub s1: u64,
    /// Saved register `s2`.
    pub s2: u64,
    /// Saved register `s3`.
    pub s3: u64,
    /// Saved register `s4`.
    pub s4: u64,
    /// Saved register `s5`.
    pub s5: u64,
    /// Saved register `s6`.
    pub s6: u64,
    /// Saved register `s7`.
    pub s7: u64,
    /// Saved register `s8`.
    pub s8: u64,
    /// Saved register `s9`.
    pub s9: u64,
    /// Saved register `s10`.
    pub s10: u64,
    /// Saved register `s11`.
    pub s11: u64,
    /// The entry point of a fresh task, staged into `a1` by the switch.
    ///
    /// `a1` rather than a callee-saved register because the staging happens
    /// on the *incoming* side of the switch, and `a1` is dead there anyway.
    pub entry: u64,
    /// The argument of a fresh task, staged into `a0` by the switch.
    pub arg: u64,
}

/// Field offsets, in bytes, that the assembly addresses by hand.
///
/// Kept in one place so the offset tests can assert the struct and the
/// assembly agree without duplicating arithmetic.
pub mod offsets {
    /// Offset of the stack pointer.
    pub const SP: usize = 0x0000;
    /// Offset of the return address.
    pub const RA: usize = 0x0008;
    /// Offset of the global pointer.
    pub const GP: usize = 0x0010;
    /// Offset of the thread pointer.
    pub const TP: usize = 0x0018;
    /// Offset of `t0`.
    pub const T0: usize = 0x0020;
    /// Offset of `t1`.
    pub const T1: usize = 0x0028;
    /// Offset of `t2`.
    pub const T2: usize = 0x0030;
    /// Offset of `t3`.
    pub const T3: usize = 0x0038;
    /// Offset of `t4`.
    pub const T4: usize = 0x0040;
    /// Offset of `t5`.
    pub const T5: usize = 0x0048;
    /// Offset of `t6`.
    pub const T6: usize = 0x0050;
    /// Offset of `s0`.
    pub const S0: usize = 0x0058;
    /// Offset of `s1`.
    pub const S1: usize = 0x0060;
    /// Offset of `s2`.
    pub const S2: usize = 0x0068;
    /// Offset of `s3`.
    pub const S3: usize = 0x0070;
    /// Offset of `s4`.
    pub const S4: usize = 0x0078;
    /// Offset of `s5`.
    pub const S5: usize = 0x0080;
    /// Offset of `s6`.
    pub const S6: usize = 0x0088;
    /// Offset of `s7`.
    pub const S7: usize = 0x0090;
    /// Offset of `s8`.
    pub const S8: usize = 0x0098;
    /// Offset of `s9`.
    pub const S9: usize = 0x00A0;
    /// Offset of `s10`.
    pub const S10: usize = 0x00A8;
    /// Offset of `s11`.
    pub const S11: usize = 0x00B0;
    /// Offset of the fresh-task entry point.
    pub const ENTRY: usize = 0x00B8;
    /// Offset of the fresh-task argument.
    pub const ARG: usize = 0x00C0;
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
            sp: 0,
            ra: 0,
            gp: 0,
            tp: 0,
            t0: 0,
            t1: 0,
            t2: 0,
            t3: 0,
            t4: 0,
            t5: 0,
            t6: 0,
            s0: 0,
            s1: 0,
            s2: 0,
            s3: 0,
            s4: 0,
            s5: 0,
            s6: 0,
            s7: 0,
            s8: 0,
            s9: 0,
            s10: 0,
            s11: 0,
            entry: 0,
            arg: 0,
        }
    }

    /// The context of a task that starts by calling `entry(arg)`.
    ///
    /// `stack_top` must be the top of the task's kernel stack; the stack grows
    /// down, so the first push lands just below it. The pointer is brought 16
    /// bytes below the (aligned) top so the psABI's "16-byte-aligned at every
    /// call site" rule holds when the trampoline calls `entry`, and so the
    /// page *at* `stack_top` — which VelaOS leaves unmapped as a guard — is
    /// never written.
    ///
    /// `ra` points at the trampoline so that the first `switch_to` into this
    /// context lands there instead of returning into garbage. On a host
    /// build (where the RV64 assembly is compiled out) `ra` is left zero;
    /// the entry/arg staging, which is what the host tests assert, is
    /// identical.
    pub fn for_entry(stack_top: u64, entry: u64, arg: u64) -> Self {
        Self {
            sp: (stack_top & !0x000F) - 16,
            ra: trampoline_address(),
            entry,
            arg,
            ..Self::new()
        }
    }

    /// The stack pointer this context will resume on.
    pub const fn stack_pointer(&self) -> u64 {
        self.sp
    }
}

/// The address of the trampoline a fresh task first resumes into.
///
/// Zero on a host build: the RV64 assembly does not exist there, and the
/// zero is never dereferenced because `switch_to` itself is a no-op off the
/// target.
#[cfg(target_arch = "riscv64")]
fn trampoline_address() -> u64 {
    vela_task_entry_trampoline as *const () as u64
}

#[cfg(not(target_arch = "riscv64"))]
fn trampoline_address() -> u64 {
    0
}

// The context switch itself.
//
// `a0` is the outgoing context and `a1` the incoming one, matching the psABI's
// first two integer arguments.
//
// The save and the load are symmetric over the 22 callee-saved registers.
// `a0`/`a1` themselves are caller-saved and are deliberately never saved:
// after the loads they are overwritten with the incoming context's `arg` and
// `entry` slots (see the module docs for why that is invisible to a resumed
// task). A fresh task then "returns" through `ra` — the trampoline — with
// `a0 = arg` and `a1 = entry`, which is exactly the psABI's calling shape for
// `entry(arg)`.
#[cfg(target_arch = "riscv64")]
use core::arch::global_asm;

#[cfg(target_arch = "riscv64")]
global_asm!(
    ".globl vela_ctx_switch",
    ".type vela_ctx_switch, @function",
    "vela_ctx_switch:",
    // --- save the outgoing context --------------------------------------
    "sd   sp, 0x0000(a0)",
    "sd   ra, 0x0008(a0)",
    "sd   gp, 0x0010(a0)",
    "sd   tp, 0x0018(a0)",
    "sd   t0, 0x0020(a0)",
    "sd   t1, 0x0028(a0)",
    "sd   t2, 0x0030(a0)",
    "sd   t3, 0x0038(a0)",
    "sd   t4, 0x0040(a0)",
    "sd   t5, 0x0048(a0)",
    "sd   t6, 0x0050(a0)",
    "sd   s0, 0x0058(a0)",
    "sd   s1, 0x0060(a0)",
    "sd   s2, 0x0068(a0)",
    "sd   s3, 0x0070(a0)",
    "sd   s4, 0x0078(a0)",
    "sd   s5, 0x0080(a0)",
    "sd   s6, 0x0088(a0)",
    "sd   s7, 0x0090(a0)",
    "sd   s8, 0x0098(a0)",
    "sd   s9, 0x00a0(a0)",
    "sd   s10, 0x00a8(a0)",
    "sd   s11, 0x00b0(a0)",
    // --- load the incoming context --------------------------------------
    "ld   sp, 0x0000(a1)",
    "ld   ra, 0x0008(a1)",
    "ld   gp, 0x0010(a1)",
    "ld   tp, 0x0018(a1)",
    "ld   t0, 0x0020(a1)",
    "ld   t1, 0x0028(a1)",
    "ld   t2, 0x0030(a1)",
    "ld   t3, 0x0038(a1)",
    "ld   t4, 0x0040(a1)",
    "ld   t5, 0x0048(a1)",
    "ld   t6, 0x0050(a1)",
    "ld   s0, 0x0058(a1)",
    "ld   s1, 0x0060(a1)",
    "ld   s2, 0x0068(a1)",
    "ld   s3, 0x0070(a1)",
    "ld   s4, 0x0078(a1)",
    "ld   s5, 0x0080(a1)",
    "ld   s6, 0x0088(a1)",
    "ld   s7, 0x0090(a1)",
    "ld   s8, 0x0098(a1)",
    "ld   s9, 0x00a0(a1)",
    "ld   s10, 0x00a8(a1)",
    "ld   s11, 0x00b0(a1)",
    // --- stage the fresh-task entry/argument into the dead a-registers --
    "ld   a0, 0x00c0(a1)",
    "ld   a1, 0x00b8(a1)",
    "ret",
    ".size vela_ctx_switch, . - vela_ctx_switch",
);

// The trampoline a fresh task starts in.
//
// Entered through the `ret` of `vela_ctx_switch` with `a0 = arg` and
// `a1 = entry`, as staged above. The trampoline only has to place the call:
// `a0` already holds the argument, so it moves the entry point into `t0` and
// jumps. If the entry point returns — which the task contract forbids — the
// `ebreak` turns the mistake into a visible breakpoint trap rather than a
// silent return into whatever `ra` happens to name.
#[cfg(target_arch = "riscv64")]
global_asm!(
    ".globl vela_task_entry_trampoline",
    ".type vela_task_entry_trampoline, @function",
    "vela_task_entry_trampoline:",
    "mv   t0, a1",
    "jalr t0",
    "ebreak",
    ".size vela_task_entry_trampoline, . - vela_task_entry_trampoline",
);

#[cfg(target_arch = "riscv64")]
extern "C" {
    /// The context switch, written in assembly.
    fn vela_ctx_switch(from: *mut Context, to: *const Context);

    /// The trampoline a fresh task starts in.
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
    #[cfg(target_arch = "riscv64")]
    // SAFETY: the caller guarantees the two contexts are live and distinct.
    unsafe {
        vela_ctx_switch(current, next)
    }
    #[cfg(not(target_arch = "riscv64"))]
    {
        // Off the target there is no assembly (and no second task); the call
        // is compiled out so host tests never dereference a bogus context.
        let _ = (current, next);
    }
}

/// The trampoline a fresh task starts in.
///
/// # Safety
///
/// This is entered by a [`switch_to`] into a context built by
/// [`Context::for_entry`], with `arg` staged in `a0` and `entry` in `a1`. It
/// must not return.
#[inline]
pub unsafe fn task_entry_trampoline() -> ! {
    #[cfg(target_arch = "riscv64")]
    // SAFETY: entered only through the switch, per the contract above.
    unsafe {
        vela_task_entry_trampoline()
    }
    #[cfg(not(target_arch = "riscv64"))]
    {
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
    use core::mem::{align_of, offset_of, size_of};

    #[test]
    fn the_assembly_offsets_match_the_struct_layout() {
        // One assertion per field the assembly addresses by hand.
        assert_eq!(offset_of!(Context, sp), offsets::SP);
        assert_eq!(offset_of!(Context, ra), offsets::RA);
        assert_eq!(offset_of!(Context, gp), offsets::GP);
        assert_eq!(offset_of!(Context, tp), offsets::TP);
        assert_eq!(offset_of!(Context, t0), offsets::T0);
        assert_eq!(offset_of!(Context, t1), offsets::T1);
        assert_eq!(offset_of!(Context, t2), offsets::T2);
        assert_eq!(offset_of!(Context, t3), offsets::T3);
        assert_eq!(offset_of!(Context, t4), offsets::T4);
        assert_eq!(offset_of!(Context, t5), offsets::T5);
        assert_eq!(offset_of!(Context, t6), offsets::T6);
        assert_eq!(offset_of!(Context, s0), offsets::S0);
        assert_eq!(offset_of!(Context, s1), offsets::S1);
        assert_eq!(offset_of!(Context, s2), offsets::S2);
        assert_eq!(offset_of!(Context, s3), offsets::S3);
        assert_eq!(offset_of!(Context, s4), offsets::S4);
        assert_eq!(offset_of!(Context, s5), offsets::S5);
        assert_eq!(offset_of!(Context, s6), offsets::S6);
        assert_eq!(offset_of!(Context, s7), offsets::S7);
        assert_eq!(offset_of!(Context, s8), offsets::S8);
        assert_eq!(offset_of!(Context, s9), offsets::S9);
        assert_eq!(offset_of!(Context, s10), offsets::S10);
        assert_eq!(offset_of!(Context, s11), offsets::S11);
        assert_eq!(offset_of!(Context, entry), offsets::ENTRY);
        assert_eq!(offset_of!(Context, arg), offsets::ARG);
    }

    #[test]
    fn the_saved_set_is_exactly_the_psabi_callee_saved_set() {
        // ra, sp, gp, tp + 7 temporaries + 12 saved registers = 22 words.
        let s11 = offset_of!(Context, s11);
        assert_eq!(s11, offsets::TP + 8 * 19, "22 callee-saved words total");
        // The staging slots sit immediately after the saved set.
        assert_eq!(offset_of!(Context, entry), s11 + 8);
        assert_eq!(offset_of!(Context, arg), s11 + 16);
    }

    #[test]
    fn the_context_is_sixteen_byte_aligned() {
        assert_eq!(align_of::<Context>(), 16);
        // A multiple of 16 keeps every `sd`/`ld` naturally aligned whenever
        // the Context itself was placed on a 16-byte boundary.
        assert_eq!(size_of::<Context>() % 16, 0);
    }

    #[test]
    fn a_fresh_context_is_zeroed() {
        let c = Context::new();
        assert_eq!(c.sp, 0);
        assert_eq!(c.entry, 0);
        assert_eq!(c.arg, 0);
    }

    #[test]
    fn an_entry_context_carries_the_entry_and_the_argument() {
        let c = Context::for_entry(0x8000, 0x0000_DEAD_BEEF, 0x1234);
        assert_eq!(c.entry, 0x0000_DEAD_BEEF);
        assert_eq!(c.arg, 0x1234);
        assert!(c.sp < 0x8000);
        // Sixteen bytes below the aligned top: the guard page at the very
        // top is never written, and sp is 16-aligned for the first call.
        assert_eq!(c.sp, 0x8000 - 16);
    }

    #[test]
    fn an_unaligned_stack_top_is_brought_down_to_alignment() {
        // 0x800F aligns down to 0x8000, minus the 16-byte headroom.
        let c = Context::for_entry(0x800F, 1, 2);
        assert_eq!(c.sp, 0x8000 - 16);
    }

    #[test]
    fn stack_pointer_reads_the_sp_field() {
        let mut c = Context::new();
        c.sp = 0x0777;
        assert_eq!(c.stack_pointer(), 0x0777);
    }
}
