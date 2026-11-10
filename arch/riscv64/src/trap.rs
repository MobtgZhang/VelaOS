//! The `stvec` trap machinery: the frame layout, the entry stub, and the
//! `scause` decoder.
//!
//! # How trapping works in S-mode
//!
//! On any trap the hardware (privileged spec, Vol. II "supervisor trap
//! entry") does exactly this, and nothing else:
//!
//! 1. Sets `scause` to the trap's cause (MSB = interrupt, low bits = code).
//! 2. Sets `stval` to the faulting address (or zero where meaningless).
//! 3. Sets `sepc` to the interrupted instruction's address.
//! 4. Saves the old `sstatus` with `SIE` cleared and `SPIE` set to the old
//!    `SIE`, and `SPP` to the privilege level the trap came from.
//! 5. Sets the program counter to `stvec` — *without* touching any general
//!    register and *without* switching stacks.
//!
//! Everything else — saving the 31 live GPRs, switching to a kernel stack,
//! deciding whether the trap is a syscall or an interrupt — is this module's
//! job, because the hardware left the interrupted registers wherever they
//! were.
//!
//! # The stack-swap discipline
//!
//! A trap from U-mode must not run on the user's stack: the program controls
//! it, and a handler that trusts it could be made to write wherever the
//! program likes. The entry stub therefore swaps `sp` with `sscratch` on
//! entry, so that `sscratch` always holds the current position on a kernel
//! trap stack — initialised with [`set_trap_stack`] before the first user
//! task runs, and left pointing below the current frame when a trap returns.
//! Traps taken in S-mode (a timer tick inside the kernel) are detected by
//! `sstatus.SPP` at entry and keep the interrupted kernel stack instead;
//! `sscratch` is restored before the handler returns so the invariant holds
//! for the next user trap either way.
//!
//! # Mechanism vs policy
//!
//! This module owns the *mechanism*: the entry stub, the frame layout and the
//! `scause` arithmetic. It does not own the *policy*: which interrupt does
//! what, and which syscall number means what, belong to the kernel. The stub
//! therefore calls two kernel-provided functions:
//!
//! * `vela_trap_handler(frame, scause, stval)` for every trap that is not an
//!   environment call from U-mode;
//! * `vela_syscall_dispatch(a0..a5, nr)` — see [`crate::syscall`] — for
//!   cause 8, returning the result and error the stub writes back into the
//!   frame's `a0`/`a1` slots before the `sret`.
//!
//! # Why the assembly is in `global_asm!`
//!
//! The hardware jumps to `stvec` with *no* register contract at all: `ra`,
//! `sp` and every scratch register still hold the interrupted context's
//! values. No Rust function can take control under those conditions — LLVM
//! assumes a function starts with a caller ABI in force — so the first
//! instructions must be assembly that establishes a frame before any
//! compiler-generated code runs.

use crate::regs::ON_BARE_METAL;

/// The register file a trap handler saves and restores.
///
/// The layout is part of the assembly contract in this module; the offset
/// tests pin the struct against the hand-written offsets. The frame holds
/// `x1`–`x31` (`x0` is hardwired zero and needs no slot), the interrupted
/// stack pointer, and the four CSRs the hardware published on entry. One
/// trailing padding word rounds the frame to 288 bytes so that a 16-byte
/// aligned trap stack stays 16-byte aligned after `addi sp, sp, -FRAME` —
/// the psABI requires `sp` to be 16-byte aligned at call sites, and the
/// Rust handlers below are ordinary calls.
#[derive(Debug, Clone, Copy)]
#[repr(C)]
pub struct TrapFrame {
    /// `ra` (x1): the interrupted return address.
    pub ra: u64,
    /// The interrupted stack pointer — user `sp` for U-mode traps, the
    /// kernel's own for S-mode traps.
    pub sp: u64,
    /// `gp` (x3).
    pub gp: u64,
    /// `tp` (x4).
    pub tp: u64,
    /// `t0` (x5).
    pub t0: u64,
    /// `t1` (x6).
    pub t1: u64,
    /// `t2` (x7).
    pub t2: u64,
    /// `t3` (x28).
    pub t3: u64,
    /// `t4` (x29).
    pub t4: u64,
    /// `t5` (x30).
    pub t5: u64,
    /// `t6` (x31).
    pub t6: u64,
    /// `s0` (x8).
    pub s0: u64,
    /// `s1` (x9).
    pub s1: u64,
    /// `s2` (x18).
    pub s2: u64,
    /// `s3` (x19).
    pub s3: u64,
    /// `s4` (x20).
    pub s4: u64,
    /// `s5` (x21).
    pub s5: u64,
    /// `s6` (x22).
    pub s6: u64,
    /// `s7` (x23).
    pub s7: u64,
    /// `s8` (x24).
    pub s8: u64,
    /// `s9` (x25).
    pub s9: u64,
    /// `s10` (x26).
    pub s10: u64,
    /// `s11` (x27).
    pub s11: u64,
    /// `a0` (x10): syscall argument 1, and the result on return.
    pub a0: u64,
    /// `a1` (x11): syscall argument 2, and the error on return.
    pub a1: u64,
    /// `a2` (x12).
    pub a2: u64,
    /// `a3` (x13).
    pub a3: u64,
    /// `a4` (x14).
    pub a4: u64,
    /// `a5` (x15).
    pub a5: u64,
    /// `a6` (x16).
    pub a6: u64,
    /// `a7` (x17): the syscall number.
    pub a7: u64,
    /// `sepc`: the address of the interrupted instruction.
    pub sepc: u64,
    /// `sstatus` as the hardware saved it on entry.
    pub sstatus: u64,
    /// `stval`: the faulting address for page faults, otherwise 0.
    pub stval: u64,
    /// `scause`: the raw cause word, interrupt bit included.
    pub scause: u64,
    /// Padding to 288 bytes: keeps a 16-byte aligned trap stack 16-byte
    /// aligned after the frame is allocated.
    pub pad: u64,
}

/// The frame's size in bytes: 36 words of 8 bytes = 288, a multiple of 16.
pub const FRAME_SIZE: usize = 288;

/// Field offsets, in bytes, that the assembly addresses by hand.
pub mod frame_offsets {
    /// Offset of `ra`.
    pub const RA: usize = 0x0000;
    /// Offset of the interrupted `sp`.
    pub const SP: usize = 0x0008;
    /// Offset of `gp`.
    pub const GP: usize = 0x0010;
    /// Offset of `tp`.
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
    /// Offset of `a0`.
    pub const A0: usize = 0x00B8;
    /// Offset of `a1`.
    pub const A1: usize = 0x00C0;
    /// Offset of `a2`.
    pub const A2: usize = 0x00C8;
    /// Offset of `a3`.
    pub const A3: usize = 0x00D0;
    /// Offset of `a4`.
    pub const A4: usize = 0x00D8;
    /// Offset of `a5`.
    pub const A5: usize = 0x00E0;
    /// Offset of `a6`.
    pub const A6: usize = 0x00E8;
    /// Offset of `a7`.
    pub const A7: usize = 0x00F0;
    /// Offset of `sepc`.
    pub const SEPC: usize = 0x00F8;
    /// Offset of `sstatus`.
    pub const SSTATUS: usize = 0x0100;
    /// Offset of `stval`.
    pub const STVAL: usize = 0x0108;
    /// Offset of `scause`.
    pub const SCAUSE: usize = 0x0110;
    /// Offset of the trailing padding word.
    pub const PAD: usize = 0x0118;
}

/// `scause`'s interrupt bit: set when the trap is an asynchronous interrupt,
/// clear for a synchronous exception.
pub const SCAUSE_INTERRUPT_BIT: u64 = 1 << 63;

/// Exception cause: instruction address misaligned.
pub const EXC_INSTRUCTION_MISALIGNED: u64 = 0;
/// Exception cause: instruction access fault.
pub const EXC_INSTRUCTION_ACCESS_FAULT: u64 = 1;
/// Exception cause: illegal instruction.
pub const EXC_ILLEGAL_INSTRUCTION: u64 = 2;
/// Exception cause: breakpoint.
pub const EXC_BREAKPOINT: u64 = 3;
/// Exception cause: load address misaligned.
pub const EXC_LOAD_MISALIGNED: u64 = 4;
/// Exception cause: load access fault.
pub const EXC_LOAD_ACCESS_FAULT: u64 = 5;
/// Exception cause: store/AMO address misaligned.
pub const EXC_STORE_MISALIGNED: u64 = 6;
/// Exception cause: store/AMO access fault.
pub const EXC_STORE_ACCESS_FAULT: u64 = 7;
/// Exception cause: environment call from U-mode — the syscall gate.
pub const EXC_ECALL_FROM_UMODE: u64 = 8;
/// Exception cause: environment call from S-mode.
pub const EXC_ECALL_FROM_SMODE: u64 = 9;
/// Exception cause: environment call from M-mode.
pub const EXC_ECALL_FROM_MMODE: u64 = 11;
/// Exception cause: instruction page fault.
pub const EXC_INSTRUCTION_PAGE_FAULT: u64 = 12;
/// Exception cause: load page fault.
pub const EXC_LOAD_PAGE_FAULT: u64 = 13;
/// Exception cause: store/AMO page fault.
pub const EXC_STORE_PAGE_FAULT: u64 = 15;
///
/// The cause numbers above are privileged spec Vol. II, "Supervisor Cause
/// and Status Registers" (`scause` table, unchanged since v1.10): 0–3 are
/// instruction-side, 4–7 the load/store pair in each of misaligned/access
/// flavours, 8–11 the environment calls per origin privilege level, and
/// 12/13/15 the three page faults (14 and 10 are reserved).
/// Interrupt cause: supervisor software interrupt.
pub const INT_SUPERVISOR_SOFTWARE: u64 = 1;
/// Interrupt cause: supervisor timer interrupt.
pub const INT_SUPERVISOR_TIMER: u64 = 5;
/// Interrupt cause: supervisor external interrupt (the PLIC's line).
pub const INT_SUPERVISOR_EXTERNAL: u64 = 9;

/// True when `cause` is an asynchronous interrupt (MSB set).
pub const fn scause_is_interrupt(cause: u64) -> bool {
    cause & SCAUSE_INTERRUPT_BIT != 0
}

/// The cause code of a `scause` word with the interrupt bit stripped.
pub const fn scause_code(cause: u64) -> u64 {
    cause & !SCAUSE_INTERRUPT_BIT
}

/// The decoded meaning of a raw `scause` word.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrapKind {
    /// An asynchronous interrupt, numbered as in the privileged spec.
    Interrupt {
        /// The interrupt's cause number.
        which: u64,
    },
    /// A synchronous exception, numbered as in the privileged spec.
    Exception {
        /// The exception's cause code.
        code: u64,
    },
}

/// Decode a raw `scause` word into [`TrapKind`].
pub const fn decode(cause: u64) -> TrapKind {
    if scause_is_interrupt(cause) {
        TrapKind::Interrupt {
            which: scause_code(cause),
        }
    } else {
        TrapKind::Exception {
            code: scause_code(cause),
        }
    }
}

/// True when the exception is one of the three page faults (codes 12, 13
/// and 15) — the ones the kernel's demand-paging path must service.
pub const fn is_page_fault(code: u64) -> bool {
    code == EXC_INSTRUCTION_PAGE_FAULT
        || code == EXC_LOAD_PAGE_FAULT
        || code == EXC_STORE_PAGE_FAULT
}

/// Build a direct-mode `stvec` value pointing at `handler`.
///
/// `stvec`'s layout (privileged spec, Vol. II, "stvec: Supervisor Trap
/// Vector Base Address Register"):
///
/// ```text
/// 63                           2 1  0
/// +-----------------------------+----+
/// |         BASE (63:2)         |MODE|
/// +-----------------------------+----+
/// ```
///
/// MODE 0 is `Direct`: every trap jumps to `BASE`. MODE 1 is `Vectored`:
/// exceptions jump to `BASE` but interrupts jump to `BASE + 4 * cause`,
/// which requires the handler to sit in a 256-entry gate table. VelaOS uses
/// direct mode and demultiplexes in software, so the stub stays one jump.
/// `handler` must be at least 4-byte aligned (function addresses are), which
/// is what keeps the mode bits and the address from colliding.
pub const fn stvec_direct(handler: u64) -> u64 {
    // MODE bits 1:0 = 0 (Direct).
    handler & !0b11
}

/// Point this hart's `stvec` at the trap entry stub.
///
/// Must be called once per hart during boot, before any user code runs and
/// before interrupts are enabled. The kernel must also have called
/// [`set_trap_stack`] first, or the first U-mode trap will run on garbage.
///
/// # Safety
///
/// Runs once per hart during boot, with the entry stub linked in.
pub unsafe fn install() {
    if !ON_BARE_METAL {
        #[cfg(not(target_arch = "riscv64"))]
        return;
    }
    #[cfg(target_arch = "riscv64")]
    {
        // The entry stub names the handler and the dispatcher by symbol.
        // Naming them here as well keeps the linker from discarding them:
        // a symbol only the assembly mentions is a symbol the linker
        // cannot prove is used.
        let _ = vela_trap_handler as *const ();
        let _ = vela_syscall_dispatch as *const ();
        let handler = stvec_direct(vela_trap_entry as *const () as u64);
        // SAFETY: writing `stvec` is always defined in S-mode; the caller
        // guarantees the boot ordering.
        unsafe {
            core::arch::asm!(
                "csrw stvec, {0}",
                in(reg) handler,
                options(nomem, nostack)
            );
        }
    }
}

/// Set the trap-stack pointer this hart's U-mode traps run on.
///
/// `top` is stored in `sscratch` by the entry stub's stack swap. The value
/// must be 16-byte aligned — the frame allocation subtracts
/// [`FRAME_SIZE`], which is a multiple of 16, and the psABI requires
/// `sp` to be 16-byte aligned at the handler's call sites.
///
/// # Safety
///
/// `top` must name the top of a mapped, kernel-owned stack that stays valid
/// for the life of the machine. Calling this while a U-mode trap is in
/// flight on this hart corrupts that trap's return path.
pub unsafe fn set_trap_stack(top: u64) {
    if !ON_BARE_METAL {
        return;
    }
    #[cfg(not(target_arch = "riscv64"))]
    let _ = top;
    #[cfg(target_arch = "riscv64")]
    // SAFETY: writing `sscratch` is always defined in S-mode; the caller
    // guarantees the alignment and lifetime of `top`.
    unsafe {
        core::arch::asm!(
            "csrw sscratch, {0}",
            in(reg) top,
            options(nomem, nostack)
        );
    }
}

#[cfg(target_arch = "riscv64")]
core::arch::global_asm!(
    r#"
    .text
    .globl vela_trap_entry
    .type vela_trap_entry, @function
vela_trap_entry:
    # Exchange sp with sscratch: sp becomes the trap-stack pointer held in
    # sscratch, and sscratch holds the interrupted stack pointer (user sp
    # for a U-mode trap, the kernel's own sp for an S-mode trap).
    csrrw sp, sscratch, sp
    addi sp, sp, -288

    # Save every live GPR. x0 is hardwired zero and needs no slot. No
    # register is clobbered before its own store: `sd` reads its source
    # register directly, and sp is the only base.
    sd   ra, 0x0000(sp)
    sd   gp, 0x0010(sp)
    sd   tp, 0x0018(sp)
    sd   t0, 0x0020(sp)
    sd   t1, 0x0028(sp)
    sd   t2, 0x0030(sp)
    sd   t3, 0x0038(sp)
    sd   t4, 0x0040(sp)
    sd   t5, 0x0048(sp)
    sd   t6, 0x0050(sp)
    sd   s0, 0x0058(sp)
    sd   s1, 0x0060(sp)
    sd   s2, 0x0068(sp)
    sd   s3, 0x0070(sp)
    sd   s4, 0x0078(sp)
    sd   s5, 0x0080(sp)
    sd   s6, 0x0088(sp)
    sd   s7, 0x0090(sp)
    sd   s8, 0x0098(sp)
    sd   s9, 0x00a0(sp)
    sd   s10, 0x00a8(sp)
    sd   s11, 0x00b0(sp)
    sd   a0, 0x00b8(sp)
    sd   a1, 0x00c0(sp)
    sd   a2, 0x00c8(sp)
    sd   a3, 0x00d0(sp)
    sd   a4, 0x00d8(sp)
    sd   a5, 0x00e0(sp)
    sd   a6, 0x00e8(sp)
    sd   a7, 0x00f0(sp)

    # The CSRs the hardware published on entry. t0 is free as scratch from
    # here on: its own value is already in the frame.
    csrr t0, sstatus
    sd   t0, 0x0100(sp)
    csrr t0, sepc
    sd   t0, 0x00f8(sp)
    csrr t0, scause
    sd   t0, 0x0110(sp)
    csrr t0, stval
    sd   t0, 0x0108(sp)

    # The interrupted stack pointer has been parked in sscratch by the
    # swap above, whichever privilege level the trap came from.
    csrr t0, sscratch
    sd   t0, 0x0008(sp)

    # Route: interrupts (scause MSB set) and every exception other than an
    # ecall from U-mode go to the kernel's generic trap handler. Cause 8 is
    # the syscall gate.
    ld   t0, 0x0110(sp)
    bltz t0, .Ltrap_generic
    andi t0, t0, 0x003f
    li   t1, 8
    bne  t0, t1, .Ltrap_generic

    # --- syscall gate ---------------------------------------------------
    # sepc still names the ecall itself; the spec does not advance it, so
    # the return address must skip the ecall instruction.
    ld   t0, 0x00f8(sp)
    addi t0, t0, 4
    sd   t0, 0x00f8(sp)
    # Call vela_syscall_dispatch(a0, a1, a2, a3, a4, a5, nr). The args are
    # already in a0-a5; the number travels from a7 (the RISC-V Linux
    # convention) to a6, the seventh argument slot per the psABI.
    ld   a0, 0x00b8(sp)
    ld   a1, 0x00c0(sp)
    ld   a2, 0x00c8(sp)
    ld   a3, 0x00d0(sp)
    ld   a4, 0x00d8(sp)
    ld   a5, 0x00e0(sp)
    ld   a6, 0x00f0(sp)
    call vela_syscall_dispatch
    # The result goes back to the user in a0 and the error in a1.
    sd   a0, 0x00b8(sp)
    sd   a1, 0x00c0(sp)
    j    .Lrestore

    # --- everything else -------------------------------------------------
.Ltrap_generic:
    # vela_trap_handler(frame, scause, stval): the frame pointer first,
    # then the two raw cause words for the handler's own decoding.
    mv   a0, sp
    ld   a1, 0x0110(sp)
    ld   a2, 0x0108(sp)
    call vela_trap_handler

    # --- restore and return ----------------------------------------------
.Lrestore:
    # Compute the post-pop trap-stack position while sp still names the
    # frame, then decide the return path from the saved SPP bit.
    addi t2, sp, 288
    ld   t0, 0x0100(sp)
    srli t0, t0, 8
    andi t0, t0, 1
    bnez t0, .Lrestore_smode

    # U-mode return: writing the saved sstatus (SPP=0) makes the sret drop
    # to user mode; the sp/sscratch swap puts the user's sp back and
    # leaves sscratch pointing below this frame, ready for the next trap.
    ld   t0, 0x0100(sp)
    csrw sstatus, t0
    ld   t0, 0x00f8(sp)
    csrw sepc, t0
    ld   ra, 0x0000(sp)
    ld   gp, 0x0010(sp)
    ld   tp, 0x0018(sp)
    ld   t0, 0x0020(sp)
    ld   t1, 0x0028(sp)
    ld   t2, 0x0030(sp)
    ld   t3, 0x0038(sp)
    ld   t4, 0x0040(sp)
    ld   t5, 0x0048(sp)
    ld   t6, 0x0050(sp)
    ld   s0, 0x0058(sp)
    ld   s1, 0x0060(sp)
    ld   s2, 0x0068(sp)
    ld   s3, 0x0070(sp)
    ld   s4, 0x0078(sp)
    ld   s5, 0x0080(sp)
    ld   s6, 0x0088(sp)
    ld   s7, 0x0090(sp)
    ld   s8, 0x0098(sp)
    ld   s9, 0x00a0(sp)
    ld   s10, 0x00a8(sp)
    ld   s11, 0x00b0(sp)
    ld   a0, 0x00b8(sp)
    ld   a1, 0x00c0(sp)
    ld   a2, 0x00c8(sp)
    ld   a3, 0x00d0(sp)
    ld   a4, 0x00d8(sp)
    ld   a5, 0x00e0(sp)
    ld   a6, 0x00e8(sp)
    ld   a7, 0x00f0(sp)
    csrrw sp, sscratch, sp
    sret

.Lrestore_smode:
    # S-mode return: sscratch must go back to the post-pop trap-stack
    # position (t2, computed above), and sp resumes the interrupted kernel
    # stack from the frame's saved slot. Writing sscratch before the GPR
    # restore keeps t2 free for its own restore.
    csrw sscratch, t2
    ld   t0, 0x0100(sp)
    csrw sstatus, t0
    ld   t0, 0x00f8(sp)
    csrw sepc, t0
    ld   ra, 0x0000(sp)
    ld   gp, 0x0010(sp)
    ld   tp, 0x0018(sp)
    ld   t0, 0x0020(sp)
    ld   t1, 0x0028(sp)
    ld   t2, 0x0030(sp)
    ld   t3, 0x0038(sp)
    ld   t4, 0x0040(sp)
    ld   t5, 0x0048(sp)
    ld   t6, 0x0050(sp)
    ld   s0, 0x0058(sp)
    ld   s1, 0x0060(sp)
    ld   s2, 0x0068(sp)
    ld   s3, 0x0070(sp)
    ld   s4, 0x0078(sp)
    ld   s5, 0x0080(sp)
    ld   s6, 0x0088(sp)
    ld   s7, 0x0090(sp)
    ld   s8, 0x0098(sp)
    ld   s9, 0x00a0(sp)
    ld   s10, 0x00a8(sp)
    ld   s11, 0x00b0(sp)
    ld   a0, 0x00b8(sp)
    ld   a1, 0x00c0(sp)
    ld   a2, 0x00c8(sp)
    ld   a3, 0x00d0(sp)
    ld   a4, 0x00d8(sp)
    ld   a5, 0x00e0(sp)
    ld   a6, 0x00e8(sp)
    ld   a7, 0x00f0(sp)
    ld   sp, 0x0008(sp)
    sret
    .size vela_trap_entry, . - vela_trap_entry
"#,
);

#[cfg(target_arch = "riscv64")]
extern "C" {
    /// The entry stub itself, so `install` can compute its address.
    fn vela_trap_entry();

    /// The kernel's generic trap handler, invoked for every trap that is
    /// not a U-mode environment call.
    fn vela_trap_handler(frame: *mut TrapFrame, scause: u64, stval: u64);

    /// The kernel's syscall dispatcher; see [`crate::syscall`].
    fn vela_syscall_dispatch(
        a0: u64,
        a1: u64,
        a2: u64,
        a3: u64,
        a4: u64,
        a5: u64,
        nr: u64,
    ) -> crate::syscall::SyscallResult;
}

#[cfg(test)]
extern crate std;

#[cfg(test)]
mod tests {
    use super::*;
    use core::mem::{offset_of, size_of};

    #[test]
    fn the_frame_offsets_match_the_struct_layout() {
        assert_eq!(offset_of!(TrapFrame, ra), frame_offsets::RA);
        assert_eq!(offset_of!(TrapFrame, sp), frame_offsets::SP);
        assert_eq!(offset_of!(TrapFrame, gp), frame_offsets::GP);
        assert_eq!(offset_of!(TrapFrame, tp), frame_offsets::TP);
        assert_eq!(offset_of!(TrapFrame, t0), frame_offsets::T0);
        assert_eq!(offset_of!(TrapFrame, t1), frame_offsets::T1);
        assert_eq!(offset_of!(TrapFrame, t2), frame_offsets::T2);
        assert_eq!(offset_of!(TrapFrame, t3), frame_offsets::T3);
        assert_eq!(offset_of!(TrapFrame, t4), frame_offsets::T4);
        assert_eq!(offset_of!(TrapFrame, t5), frame_offsets::T5);
        assert_eq!(offset_of!(TrapFrame, t6), frame_offsets::T6);
        assert_eq!(offset_of!(TrapFrame, s0), frame_offsets::S0);
        assert_eq!(offset_of!(TrapFrame, s1), frame_offsets::S1);
        assert_eq!(offset_of!(TrapFrame, s2), frame_offsets::S2);
        assert_eq!(offset_of!(TrapFrame, s3), frame_offsets::S3);
        assert_eq!(offset_of!(TrapFrame, s4), frame_offsets::S4);
        assert_eq!(offset_of!(TrapFrame, s5), frame_offsets::S5);
        assert_eq!(offset_of!(TrapFrame, s6), frame_offsets::S6);
        assert_eq!(offset_of!(TrapFrame, s7), frame_offsets::S7);
        assert_eq!(offset_of!(TrapFrame, s8), frame_offsets::S8);
        assert_eq!(offset_of!(TrapFrame, s9), frame_offsets::S9);
        assert_eq!(offset_of!(TrapFrame, s10), frame_offsets::S10);
        assert_eq!(offset_of!(TrapFrame, s11), frame_offsets::S11);
        assert_eq!(offset_of!(TrapFrame, a0), frame_offsets::A0);
        assert_eq!(offset_of!(TrapFrame, a1), frame_offsets::A1);
        assert_eq!(offset_of!(TrapFrame, a2), frame_offsets::A2);
        assert_eq!(offset_of!(TrapFrame, a3), frame_offsets::A3);
        assert_eq!(offset_of!(TrapFrame, a4), frame_offsets::A4);
        assert_eq!(offset_of!(TrapFrame, a5), frame_offsets::A5);
        assert_eq!(offset_of!(TrapFrame, a6), frame_offsets::A6);
        assert_eq!(offset_of!(TrapFrame, a7), frame_offsets::A7);
        assert_eq!(offset_of!(TrapFrame, sepc), frame_offsets::SEPC);
        assert_eq!(offset_of!(TrapFrame, sstatus), frame_offsets::SSTATUS);
        assert_eq!(offset_of!(TrapFrame, stval), frame_offsets::STVAL);
        assert_eq!(offset_of!(TrapFrame, scause), frame_offsets::SCAUSE);
        assert_eq!(offset_of!(TrapFrame, pad), frame_offsets::PAD);
    }

    #[test]
    fn the_frame_size_keeps_the_stack_sixteen_byte_aligned() {
        // 31 GPRs + interrupted sp + 4 CSRs + 1 pad = 36 words of 8 bytes.
        assert_eq!(size_of::<TrapFrame>(), FRAME_SIZE);
        assert_eq!(FRAME_SIZE % 16, 0);
    }

    #[test]
    fn the_frame_holds_every_register_except_x0() {
        // x0 is hardwired zero: ra, sp, gp, tp, 7 t-regs, 12 s-regs, 8 a-regs.
        let saved = 4 + 7 + 12 + 8;
        assert_eq!(saved, 31);
        assert_eq!(size_of::<TrapFrame>() / 8, 31 + 4 + 1);
    }

    #[test]
    fn the_interrupt_bit_is_the_msb() {
        assert!(scause_is_interrupt(1 << 63 | 5));
        assert!(!scause_is_interrupt(5));
        assert_eq!(scause_code(1 << 63 | 5), 5, "code masks the MSB off");
        assert_eq!(scause_code(5), 5);
    }

    #[test]
    fn decode_routes_interrupts_and_exceptions() {
        assert_eq!(
            decode(1 << 63 | INT_SUPERVISOR_TIMER),
            TrapKind::Interrupt {
                which: INT_SUPERVISOR_TIMER
            }
        );
        assert_eq!(
            decode(EXC_ECALL_FROM_UMODE),
            TrapKind::Exception {
                code: EXC_ECALL_FROM_UMODE
            }
        );
    }

    #[test]
    fn page_faults_are_exactly_codes_12_13_and_15() {
        for code in [
            EXC_INSTRUCTION_PAGE_FAULT,
            EXC_LOAD_PAGE_FAULT,
            EXC_STORE_PAGE_FAULT,
        ] {
            assert!(is_page_fault(code));
        }
        // The neighbours are not page faults: access faults are PMP/PMA
        // problems, and ecall 8 is the syscall gate.
        for code in [
            EXC_INSTRUCTION_ACCESS_FAULT,
            EXC_LOAD_ACCESS_FAULT,
            EXC_STORE_ACCESS_FAULT,
            EXC_ECALL_FROM_UMODE,
        ] {
            assert!(!is_page_fault(code));
        }
    }

    #[test]
    fn the_smode_cause_set_matches_the_privileged_spec() {
        // Interrupts: 1 = software, 5 = timer, 9 = external.
        assert_eq!(INT_SUPERVISOR_SOFTWARE, 1);
        assert_eq!(INT_SUPERVISOR_TIMER, 5);
        assert_eq!(INT_SUPERVISOR_EXTERNAL, 9);
        // Exceptions, in the spec's order.
        assert_eq!(EXC_INSTRUCTION_MISALIGNED, 0);
        assert_eq!(EXC_INSTRUCTION_ACCESS_FAULT, 1);
        assert_eq!(EXC_ILLEGAL_INSTRUCTION, 2);
        assert_eq!(EXC_BREAKPOINT, 3);
        assert_eq!(EXC_LOAD_MISALIGNED, 4);
        assert_eq!(EXC_LOAD_ACCESS_FAULT, 5);
        assert_eq!(EXC_STORE_MISALIGNED, 6);
        assert_eq!(EXC_STORE_ACCESS_FAULT, 7);
        assert_eq!(EXC_ECALL_FROM_UMODE, 8);
        assert_eq!(EXC_ECALL_FROM_SMODE, 9);
        assert_eq!(EXC_ECALL_FROM_MMODE, 11);
        assert_eq!(EXC_INSTRUCTION_PAGE_FAULT, 12);
        assert_eq!(EXC_LOAD_PAGE_FAULT, 13);
        assert_eq!(EXC_STORE_PAGE_FAULT, 15);
    }

    #[test]
    fn stvec_direct_mode_clears_the_mode_field() {
        // A handler at an 8-byte-aligned address: the direct-mode value is
        // the address itself, because MODE 0 is the all-zero encoding.
        assert_eq!(stvec_direct(0x1000), 0x1000);
        assert_eq!(stvec_direct(0x1234), 0x1234 & !0b11);
        // Vectored mode would be 0b01; the builder always produces Direct.
        assert_eq!(stvec_direct(0x1235) & 0b11, 0);
    }

    #[test]
    fn exception_codes_are_distinct() {
        let all = [
            EXC_INSTRUCTION_MISALIGNED,
            EXC_INSTRUCTION_ACCESS_FAULT,
            EXC_ILLEGAL_INSTRUCTION,
            EXC_BREAKPOINT,
            EXC_LOAD_MISALIGNED,
            EXC_LOAD_ACCESS_FAULT,
            EXC_STORE_MISALIGNED,
            EXC_STORE_ACCESS_FAULT,
            EXC_ECALL_FROM_UMODE,
            EXC_ECALL_FROM_SMODE,
            EXC_ECALL_FROM_MMODE,
            EXC_INSTRUCTION_PAGE_FAULT,
            EXC_LOAD_PAGE_FAULT,
            EXC_STORE_PAGE_FAULT,
        ];
        for (i, a) in all.iter().enumerate() {
            for b in all.iter().skip(i + 1) {
                assert_ne!(a, b, "duplicate exception code");
            }
        }
    }
}
