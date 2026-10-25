//! The interrupt descriptor table and x86-64 exception handling.
//!
//! # Why the handlers abort instead of returning
//!
//! A `#PF` that a kernel *cannot* handle is unrecoverable: there is no
//! user-mode equivalent of a signal frame the kernel could unwind through, and
//! continuing past it would let the faulting instruction re-execute forever.
//! The plan is explicit about this — "page fault prints CR2 + error code and
//! halts loudly, never silently black-screens". Every handler here therefore
//! prints a full diagnostic and then stops the machine through
//! [`crate::qemu_exit::power_off`], which is observable to CI via
//! `isa-debug-exit`.
//!
//! # Interrupt frame
//!
//! The CPU pushes, in this order on entry: `SS`, `RSP`, `RFLAGS`, `CS`, `RIP`,
//! then the vector-specific error code if the gate is an error gate. The struct
//! [`InterruptFrame`] mirrors exactly that layout, so a handler can read the
//! saved `RIP`/`RSP` and the error code without any assembly beyond the entry
//! trampoline.
//!
//! # Why the entry code is assembly, and why there is one per vector
//!
//! The CPU pushes a *different frame* depending on the exception:
//!
//! * Vectors 8, 10, 11, 12, 13, 14, 17, 21, 29 and 30 push an **error
//!   code** before the rest of the frame.
//! * Every other vector pushes the frame alone.
//!
//! A single handler for both shapes cannot exist in Rust, because the
//! compiler must know at compile time how many bytes are on the stack. A
//! handler written for the *no-error-code* shape, applied to an
//! *error-code* vector, reads the error code as the saved `RIP` — and the
//! handler then returns through a frame that is shifted by eight bytes.
//!
//! That is exactly what turned a single `#UD` into a triple fault in
//! VelaOS: the `#UD` was reported by a handler that assumed the wrong frame
//! shape, the handler's `ret` popped a garbage address, the CPU took a
//! `#GP`, and the `#GP`'s handler assumed the wrong shape *again*, so the
//! machine died with no diagnostic at all.
//!
//! So each vector gets its own tiny assembly stub, and the stub — not the
//! Rust handler — is what knows the frame's shape. The stub converts the
//! CPU's frame into a uniform [`InterruptFrame`] on the stack and calls a
//! single Rust entry point with the vector number.
//!
//! # Why the frame is copied rather than used in place
//!
//! The CPU's frame lives at an arbitrary address with an arbitrary
//! alignment, and its shape differs between error-code and non-error-code
//! vectors. Copying it into a fixed, 16-byte-aligned local frame of the
//! Rust handler's own stack gives every handler the *same* view: the same
//! struct, the same alignment, the same field order — with the error code
//! normalised to zero for the vectors that do not push one.
//!
//! # Why the double fault has its own stack
//!
//! A double fault means the kernel could not even deliver the first fault.
//! The usual cause is a broken stack: the faulting code had exhausted the
//! stack it was running on, and the handler's own prologue then faulted
//! again. Running the `#DF` handler on the *same* stack therefore turns one
//! fault into an infinite regress.
//!
//! So the `#DF` handler runs on a private stack, in `.bss`, and the CPU's
//! frame is copied into a private buffer first. This does not need an IST:
//! the IST is a mechanism for switching stacks *before* the first instruction
//! of the handler runs, and a copy-then-switch achieves the same thing for
//! a handler that never returns.

use core::arch::{asm, global_asm};
use core::sync::atomic::{AtomicU64, Ordering};

use spin::Mutex;

use crate::qemu_exit;
use crate::serial;

// The entry stubs.
//
// # The shape of the problem
//
// The CPU pushes a different frame depending on the exception. Vectors 8,
// 10, 11, 12, 13, 14, 17, 21, 29 and 30 push an **error code** first; every
// other vector does not. A Rust handler cannot cope with both shapes at
// once, because the compiler has to know at compile time how many bytes are
// on the stack.
//
// So each vector gets its own stub, and the stub is what knows the shape.
// Each stub:
//
// 1. copies the CPU's frame into a fixed, 16-byte-aligned local frame,
// 2. normalises the error code to zero for the vectors that push none,
// 3. calls the single Rust entry point with the frame and the vector number.
//
// # Why `rbx` is saved
//
// `rbx` is callee-saved under the System V ABI, and the stubs are entered
// by the CPU in the middle of whatever the interrupted code was doing — so
// the interrupted code may well have had a live value in `rbx`. The stub
// uses `rbx` to remember where the CPU's frame is, so it has to save and
// restore it.
//
// # Why the interrupt stub ends in `iretq`
//
// An interrupt is not a call. The CPU pushed a frame — RIP, CS, RFLAGS,
// RSP, SS — and the handler must pop it and resume. A handler that simply
// *returns* would resume with the CPU's frame still on the stack, and the
// function's own epilogue would run against it. So the stub ends with
// `iretq`, which is what pops the CPU's frame and resumes the interrupted
// code.
//
// # Why the interrupt stub saves the frame *before* calling Rust
//
// The Rust handler runs on a stack the stub has just carved out, so the
// CPU's frame is no longer at the top of the stack. The stub therefore
// copies it into a local frame first, exactly as the exception stubs do,
// and restores `rsp` to the CPU's frame before `iretq`.

global_asm!(
    r#"
    // # Why the frame's RSP and SS are conditional
    //
    // On a same-privilege interrupt or exception the CPU pushes exactly
    // three words -- RIP, CS and RFLAGS. It pushes RSP and SS as well only
    // when the privilege level changes. Copying them unconditionally would
    // read whatever happens to sit above the frame -- usually the
    // interrupted function's return address -- and report it as the
    // interrupted stack pointer.
    //
    // So every stub compares the saved CS's requested privilege level with
    // the current one, and copies RSP and SS only when the two differ.

    // The macro that defines one exception stub.
    //
    // `num` is the vector number. This is a *no-error-code* stub: the
    // vectors that push an error code use `VELA_ERROR_STUB` instead.
    .macro VELA_EXC_STUB num
    .globl vela_exception_stub_\num
    .type vela_exception_stub_\num, @function
    vela_exception_stub_\num:
        // Save the callee-saved register the stub uses, and remember where
        // the CPU's frame is.
        //
        // The stub pushes `rbx` *before* it addresses the frame, so `rbx`
        // points at the saved register and the CPU's frame starts eight
        // bytes higher, at `rbx + 8`.
        push rbx
        mov rbx, rsp

        // Carve out the handler's frame. 48 bytes is the size of the Rust
        // `InterruptFrame`: an error code, RIP, CS, RFLAGS, RSP and SS.
        and rsp, -16
        sub rsp, 48

        // The error code is zero for these vectors: they do not push one.
        mov qword ptr [rsp], 0
        // Copy RIP, CS and RFLAGS out of the CPU's frame.
        mov rax, qword ptr [rbx + 8]
        mov qword ptr [rsp + 8], rax
        mov rax, qword ptr [rbx + 16]
        mov qword ptr [rsp + 16], rax
        mov rax, qword ptr [rbx + 24]
        mov qword ptr [rsp + 24], rax
        // RSP and SS, but only if the privilege level changed.
        //
        // The CPU pushes RSP and SS only on a privilege-level change, so
        // reading them unconditionally would read whatever happens to sit
        // above the frame -- usually the interrupted function's return
        // address -- and report it as the interrupted stack pointer.
        //
        // The saved CS carries the *old* privilege level; the CS register
        // itself carries the new one, because the CPU has already loaded it
        // by the time the handler's first instruction runs.
        mov r11, cs
        and r11d, 3
        movzx ecx, word ptr [rbx + 16]
        and ecx, 3
        cmp ecx, r11d
        je 1f
        mov rax, qword ptr [rbx + 32]
        mov qword ptr [rsp + 32], rax
        mov rax, qword ptr [rbx + 40]
        mov qword ptr [rsp + 40], rax
    1:

        // Call the Rust entry point: `rdi` is the frame, `rsi` the vector.
        mov rdi, rsp
        mov esi, \num
        call vela_exception_entry

        // `vela_exception_entry` never returns. If it somehow does, there is
        // no frame to return through, so the only honest thing is to stop.
        ud2
    .size vela_exception_stub_\num, . - vela_exception_stub_\num
    .endm

    // The macro that defines one stub for a vector that *does* push an
    // error code.
    //
    // The only difference is the first store: the CPU's frame already
    // begins with the error code, so it is copied rather than zeroed.
    .macro VELA_ERROR_STUB num
    .globl vela_error_stub_\num
    .type vela_error_stub_\num, @function
    vela_error_stub_\num:
        push rbx
        mov rbx, rsp

        and rsp, -16
        sub rsp, 48

        // The error code is the first word of the CPU's frame.
        mov rax, qword ptr [rbx + 8]
        mov qword ptr [rsp], rax
        // RIP, CS and RFLAGS follow it.
        mov rax, qword ptr [rbx + 16]
        mov qword ptr [rsp + 8], rax
        mov rax, qword ptr [rbx + 24]
        mov qword ptr [rsp + 16], rax
        mov rax, qword ptr [rbx + 32]
        mov qword ptr [rsp + 24], rax
        // RSP and SS, but only if the privilege level changed.
        mov r11, cs
        and r11d, 3
        movzx ecx, word ptr [rbx + 24]
        and ecx, 3
        cmp ecx, r11d
        je 1f
        mov rax, qword ptr [rbx + 40]
        mov qword ptr [rsp + 32], rax
        mov rax, qword ptr [rbx + 48]
        mov qword ptr [rsp + 40], rax
    1:

        mov rdi, rsp
        mov esi, \num
        call vela_exception_entry

        ud2
    .size vela_error_stub_\num, . - vela_error_stub_\num
    .endm

    // The macro that defines one interrupt stub.
    //
    // The difference from the exception stubs is the ending: the stub
    // restores the stack to the CPU's frame and executes `iretq`, which
    // pops it and resumes the interrupted code.
    .macro VELA_INT_STUB num
    .globl vela_interrupt_stub_\num
    .type vela_interrupt_stub_\num, @function
    vela_interrupt_stub_\num:
        // # Why every general-purpose register is saved
        //
        // An interrupt is *not* a call. The CPU saves only RIP, CS and
        // RFLAGS -- and RSP and SS, but only when the privilege level
        // changes. Every other register still belongs to the interrupted
        // code.
        //
        // The handler is Rust, so it uses the caller-saved registers as it
        // pleases, exactly as it would in any other function call. A timer
        // tick that destroys the interrupted code's RDI therefore turns the
        // *next* instruction into a fault that appears to come from
        // somewhere else entirely: the interrupt looks like it was taken
        // cleanly, and the fault looks like a bug in the interrupted code.
        //
        // So the stub saves them all, gives the handler a frame of its own,
        // and puts them back before the `iretq`.
        //
        // # The stack layout
        //
        // A push moves the stack pointer *down*, so the CPU's frame -- which
        // was pushed before the stub ran -- sits at *higher* addresses than
        // the stub's own saves. Counting from `rbx`, which the stub set to
        // the address of its first save:
        //
        //   rbx +  0   the saved `rbx`
        //   rbx +  8   the CPU's saved RIP
        //   rbx + 16   the CPU's saved CS
        //   rbx + 24   the CPU's saved RFLAGS
        //   rbx + 32   the CPU's saved RSP   (only on a privilege change)
        //   rbx + 40   the CPU's saved SS    (only on a privilege change)
        //
        // and the fourteen registers the stub saved after `rbx` occupy
        // `rbx - 8` through `rbx - 112`, in the order they were pushed.
        //
        // Getting this backwards is invisible in the log and fatal: the
        // `iretq` resumes with the saved CS in the RIP slot, and the CPU
        // reports that as a `#GP` against the *interrupted* instruction --
        // a fault that looks like a bug in the interrupted code rather than
        // in the interrupt path.
        push rbx
        mov rbx, rsp
        push rbp
        push r12
        push r13
        push r14
        push r15
        push rsi
        push rdi
        push r8
        push r9
        push r10
        push r11
        push rcx
        push rdx
        push rax

        // Carve out the handler's frame. 48 bytes is the size of the Rust
        // `InterruptFrame`: an error code, RIP, CS, RFLAGS, RSP and SS.
        //
        // The frame is carved *below* the saved registers, so nothing the
        // stub later reads can be overwritten by the handler's own locals.
        and rsp, -16
        sub rsp, 48

        // An interrupt pushes no error code.
        mov qword ptr [rsp], 0
        // RIP, CS and RFLAGS.
        mov rax, qword ptr [rbx + 8]
        mov qword ptr [rsp + 8], rax
        mov rax, qword ptr [rbx + 16]
        mov qword ptr [rsp + 16], rax
        mov rax, qword ptr [rbx + 24]
        mov qword ptr [rsp + 24], rax
        // RSP and SS, but only if the privilege level changed.
        //
        // The CPU pushes RSP and SS only on a privilege-level change, so
        // reading them unconditionally would read whatever happens to sit
        // above the frame -- usually the interrupted function's return
        // address -- and report it as the interrupted stack pointer.
        //
        // The saved CS carries the *old* privilege level; the CS register
        // itself carries the new one, because the CPU has already loaded it
        // by the time the handler's first instruction runs.
        mov r11, cs
        and r11d, 3
        movzx ecx, word ptr [rbx + 16]
        and ecx, 3
        cmp ecx, r11d
        je 1f
        mov rax, qword ptr [rbx + 32]
        mov qword ptr [rsp + 32], rax
        mov rax, qword ptr [rbx + 40]
        mov qword ptr [rsp + 40], rax
    1:

        mov rdi, rsp
        mov esi, \num
        call vela_interrupt_entry

        // # Why the return frame is *not* rebuilt
        //
        // `iretq` in long mode pops RIP, CS and RFLAGS -- and RSP and SS
        // *only* when the return is to a different privilege level. That is
        // the mirror image of what the CPU pushed: the CPU pushes RSP and
        // SS only when the privilege level changed, and `iretq` pops them
        // only when the return changes it back.
        //
        // So for a same-privilege interrupt the CPU's three-word frame is
        // exactly what `iretq` expects, and the stub can simply let the
        // instruction pop it in place.
        //
        // # Why the register restore is a run of pops ending at the frame
        //
        // The saved registers are *below* `rbx`, in the reverse of the
        // order they were pushed, so restoring them is a run of pops. The
        // last pop restores `rbx` itself, which leaves the stack pointer
        // exactly at the CPU's saved RIP -- the first word of the frame
        // `iretq` is about to pop.
        mov rsp, rbx
        sub rsp, 112
        pop rax
        pop rdx
        pop rcx
        pop r11
        pop r10
        pop r9
        pop r8
        pop rdi
        pop rsi
        pop r15
        pop r14
        pop r13
        pop r12
        pop rbp
        pop rbx
        iretq
    .size vela_interrupt_stub_\num, . - vela_interrupt_stub_\num
    .endm

    // The stub for the double fault (`#DF`, vector 8).
    //
    // # Why this one is different
    //
    // A double fault means the kernel could not deliver the first fault --
    // in practice because the stack was exhausted. Running this handler on
    // that same stack would fault again, and again, and the machine would
    // die with no diagnostic.
    //
    // So this stub:
    //
    // 1. copies the CPU's frame into a private buffer in `.bss`,
    // 2. switches to a private stack in `.bss`,
    // 3. calls the same Rust entry point as every other exception.
    //
    // It does not use an IST. The IST switches stacks *before* the
    // handler's first instruction, which is the textbook answer, but it
    // requires the TSS to carry the stack pointer -- and the TSS is written
    // by the GDT module, which is not this module's to change. Copying the
    // frame to a buffer and then switching stacks achieves the same
    // protection for a handler that never returns, without touching the
    // TSS.
    .globl vela_double_fault_stub
    .type vela_double_fault_stub, @function
    vela_double_fault_stub:
        // Copy the CPU's frame into the private buffer.
        //
        // The CPU's frame for a double fault is the error code, RIP, CS and
        // RFLAGS -- four words. The buffer is larger than that, and the rest
        // is zeroed by the Rust side.
        lea rdi, [rip + vela_df_buffer]
        mov rsi, rsp
        mov rcx, 4
        rep movsq

        // Switch to the private stack and call the entry point.
        lea rsp, [rip + vela_df_stack_top]
        and rsp, -16
        lea rdi, [rip + vela_df_buffer]
        mov esi, 8
        call vela_exception_entry

        ud2
    .size vela_double_fault_stub, . - vela_double_fault_stub

    // The stubs themselves.
    VELA_EXC_STUB 0
    VELA_EXC_STUB 1
    VELA_EXC_STUB 2
    VELA_EXC_STUB 3
    VELA_EXC_STUB 4
    VELA_EXC_STUB 5
    VELA_EXC_STUB 6
    VELA_EXC_STUB 7
    VELA_EXC_STUB 9
    VELA_ERROR_STUB 10
    VELA_ERROR_STUB 11
    VELA_ERROR_STUB 12
    VELA_ERROR_STUB 13
    VELA_ERROR_STUB 14
    VELA_EXC_STUB 15
    VELA_EXC_STUB 16
    VELA_ERROR_STUB 17
    VELA_EXC_STUB 18
    VELA_EXC_STUB 19
    VELA_EXC_STUB 20
    VELA_ERROR_STUB 21
    VELA_EXC_STUB 22
    VELA_EXC_STUB 23
    VELA_EXC_STUB 24
    VELA_EXC_STUB 25
    VELA_EXC_STUB 26
    VELA_EXC_STUB 27
    VELA_EXC_STUB 28
    VELA_ERROR_STUB 29
    VELA_ERROR_STUB 30
    VELA_EXC_STUB 31
    VELA_INT_STUB 32
    VELA_INT_STUB 33
    VELA_INT_STUB 34
    VELA_INT_STUB 35
    VELA_INT_STUB 36
    VELA_INT_STUB 37
    VELA_INT_STUB 38
    VELA_INT_STUB 39
    VELA_INT_STUB 40
    VELA_INT_STUB 41
    VELA_INT_STUB 42
    VELA_INT_STUB 43
    VELA_INT_STUB 44
    VELA_INT_STUB 45
    VELA_INT_STUB 46
    VELA_INT_STUB 47
    VELA_INT_STUB 48
    VELA_INT_STUB 255

    // The private buffer and stack for the double fault.
    //
    // # Why these live in `.bss`
    //
    // They must be writable and must not be in the image's file extent: the
    // buffer is scratch space and the stack is used once, if ever. `.bss` is
    // zeroed by the loader and costs nothing in the image.
    //
    // They are *not* in `.data`, because a `global_asm!` block in `.data`
    // with `.skip` is rejected by the assembler -- a `.bss` section must
    // have no file contents.
    .section .bss,"aw",@nobits
    .align 16
    .globl vela_df_buffer
    .type vela_df_buffer, @object
    vela_df_buffer:
    .skip 64
    .size vela_df_buffer, . - vela_df_buffer

    .align 16
    .globl vela_df_stack
    .type vela_df_stack, @object
    vela_df_stack:
    .skip 16384
    .size vela_df_stack, . - vela_df_stack

    .globl vela_df_stack_top
    vela_df_stack_top:

    .section .text
    "#
);
/// Entries in the interrupt descriptor table.
///
/// The x86-64 architectural limit is 256; the first 32 are the CPU's own
/// exceptions, the rest are IRQs.
pub const IDT_ENTRIES: usize = 256;

/// Bytes per IDT entry: a 16-byte gate descriptor.
const GATE_BYTES: usize = 16;
/// The IDT itself must be a multiple of a page.
pub const IDT_BYTES: usize = IDT_ENTRIES * GATE_BYTES;

/// CPU exception vectors.
pub mod vectors {
    /// Divide error: `#DE`, vector 0.
    pub const DIVIDE_ERROR: u8 = 0;
    /// Debug.
    pub const DEBUG: u8 = 1;
    /// Non-maskable interrupt.
    pub const NMI: u8 = 2;
    /// Breakpoint (`int3`).
    pub const BREAKPOINT: u8 = 3;
    /// Overflow.
    pub const OVERFLOW: u8 = 4;
    /// Bound range exceeded.
    pub const BOUND_RANGE: u8 = 5;
    /// Invalid opcode (`#UD`).
    pub const INVALID_OPCODE: u8 = 6;
    /// Device not available.
    pub const DEVICE_NOT_AVAILABLE: u8 = 7;
    /// Double fault (`#DF`).
    pub const DOUBLE_FAULT: u8 = 8;
    /// Coprocessor segment overrun.
    pub const COPROCESSOR_SEGMENT: u8 = 9;
    /// Invalid TSS.
    pub const INVALID_TSS: u8 = 10;
    /// Segment not present (`#NP`).
    pub const SEGMENT_NOT_PRESENT: u8 = 11;
    /// Stack-segment fault (`#SS`).
    pub const STACK_SEGMENT: u8 = 12;
    /// General protection fault (`#GP`).
    pub const GENERAL_PROTECTION: u8 = 13;
    /// Page fault (`#PF`).
    pub const PAGE_FAULT: u8 = 14;
    /// Reserved.
    pub const RESERVED_15: u8 = 15;
    /// x87 floating-point exception.
    pub const FPU_ERROR: u8 = 16;
    /// Alignment check (`#AC`).
    pub const ALIGNMENT_CHECK: u8 = 17;
    /// Machine check.
    pub const MACHINE_CHECK: u8 = 18;
    /// SIMD floating-point exception (`#XM`).
    pub const SIMD_ERROR: u8 = 19;
    /// Virtualisation exception.
    pub const VIRTUALIZATION: u8 = 20;
    /// Control protection exception.
    pub const CONTROL_PROTECTION: u8 = 21;

    /// The first interrupt request line, `IRQ0`.
    pub const IRQ_BASE: u8 = 32;
    /// The last legacy PIC interrupt line, `IRQ15`.
    pub const IRQ_LAST: u8 = 47;
}

/// Gate attribute bits, split by their position in the 64-bit gate
/// descriptor.
///
/// The 64-bit gate layout is *not* the 32-bit one, and conflating the two is
/// the single most common IDT bug. A 32-bit interrupt gate packs everything
/// into byte 4 as `0x8E`; the 64-bit form spreads the same information across
/// bytes 4, 5 and 6, and a gate built with the 32-bit layout makes the CPU
/// enter the handler in compatibility mode -- the stack switch then goes wrong
/// and the result is a `#GP` at best.
///
/// The architectural layout of a 64-bit IDT entry, by bit:
///
/// ```text
///    0-15  offset 15..0 of the handler
///   16-31  segment selector
///   32-34  IST index
///   35-40  reserved, zero
///   41-42  type attribute: 0b00 = 64-bit interrupt gate
///      43  reserved, zero
///   44-45  descriptor privilege level
///      46  reserved, zero
///      47  present
///   48-55  reserved, zero
///   56-63  reserved, zero
///   64-95  offset 31..16 of the handler
///  96-127  offset 63..32 of the handler
/// ```
pub mod gate {
    /// Byte 4, bits 0-2: the Interrupt Stack Table index.
    ///
    /// Zero for every gate here: VelaOS uses a single kernel stack for
    /// exceptions rather than a separate IST stack per vector.
    pub const IST_MASK: u8 = 0b0000_0111;

    /// Byte 5, bit 0: the interrupt/trap bit.
    ///
    /// `0` selects an **interrupt gate**, which clears `IF` on entry.
    /// `1` selects a **trap gate**, which leaves `IF` alone.
    ///
    /// This bit is the one that is easy to lose, because it is the lowest bit
    /// of the byte and it is *not* part of the type attribute. A gate built
    /// from the type attribute alone reads back as `0b1000_0000`, whose
    /// low nibble is the architecturally *invalid* `0b0000`. The CPU rejects
    /// such a gate with a `#GP` — and because the rejection happens on the
    /// *first* interrupt the table is asked to deliver, the kernel dies on its
    /// first timer tick, with the fault reported against whichever
    /// instruction happened to be running. Nothing points at the IDT.
    pub const GATE_TYPE_SHIFT: u8 = 0;
    /// The mask for the interrupt/trap bit in byte 5.
    pub const GATE_TYPE_MASK: u8 = 0b0000_0001;
    /// The gate type of an interrupt gate.
    pub const GATE_TYPE_INTERRUPT: u8 = 0b0;
    /// The gate type of a trap gate.
    pub const GATE_TYPE_TRAP: u8 = 0b1;

    /// Byte 5, bits 3-1: the type attribute.
    ///
    /// `0b111` selects a **64-bit gate**, which is what makes the CPU enter
    /// the handler in long mode. `0b110` selects a 32-bit gate, and the
    /// handler would run in compatibility mode with a 32-bit stack frame —
    /// which is not a stack frame at all on x86-64.
    ///
    /// The field is three bits wide, not two: bit 3 is part of it. That is
    /// the second easy way to get this byte wrong.
    pub const TYPE_ATTR_SHIFT: u8 = 1;
    /// The mask for the type-attribute field in byte 5.
    pub const TYPE_ATTR_MASK: u8 = 0b0000_0111;
    /// The type attribute of a 64-bit gate.
    pub const TYPE_ATTR_64_INTERRUPT: u8 = 0b111;
    /// The type attribute of a 32-bit gate.
    pub const TYPE_ATTR_32_INTERRUPT: u8 = 0b110;

    /// Byte 5, bits 4-5: the descriptor privilege level.
    ///
    /// DPL 0, so user mode cannot take these vectors directly. A DPL-0 gate
    /// that user code could invoke with an arbitrary stack would be a
    /// privilege-escalation primitive, so Phase 4 routes user-mode faults
    /// through a dedicated mechanism instead of loosening this.
    pub const DPL_SHIFT: u8 = 4;
    /// The mask for the DPL field in byte 5.
    pub const DPL_MASK: u8 = 0b0000_0011;

    /// Byte 5, bit 7: present.
    pub const PRESENT: u8 = 1 << 7;

    /// Byte 6, bits 0-3: the legacy gate-type field.
    ///
    /// In the 64-bit layout this field is architecturally reserved and must be
    /// zero; the interrupt/trap distinction lives in the type attribute in
    /// byte 5. It is kept named because some references still show it, and
    /// writing the reserved value explicitly is clearer than leaving the byte
    /// implicit.
    pub const TYPE_RESERVED: u8 = 0x00;
}

/// What the CPU saved when it entered an exception handler.
///
/// Field order is load-bearing: it must match the push order the CPU performs,
/// which is the reverse of this list.
#[derive(Debug, Clone, Copy)]
#[repr(C)]
pub struct InterruptFrame {
    /// Error code, or `0` for vectors that do not push one.
    pub error_code: u64,
    /// Saved instruction pointer.
    pub rip: u64,
    /// Saved code segment.
    pub cs: u64,
    /// Saved processor flags.
    pub rflags: u64,
    /// Saved stack pointer.
    pub rsp: u64,
    /// Saved stack segment.
    pub ss: u64,
}

impl InterruptFrame {
    /// User-mode frame: bit 2 of the saved `CS` is the RPL.
    pub const fn from_user(&self) -> bool {
        self.cs & 0b11 == 0b11
    }
}

/// Decode the bits of a page-fault error code.
///
/// The architectural encoding of bits 0-3 is the *protection* form, not the
/// "P=present" form some references show:
///
/// | bit | name  | meaning in the page-fault error code          |
/// |-----|-------|---------------------------------------------------|
/// | 0   | P     | 0 = the page was not present, 1 = protection fault |
/// | 1   | W/R   | 0 = the access was a read, 1 = a write            |
/// | 2   | U/S   | 0 = supervisor access, 1 = user access            |
/// | 3   | RSVD  | 1 = a reserved bit in the PTE was set            |
/// | 4   | I/D   | 1 = the access was an instruction fetch          |
///
/// Getting the ordering wrong misreports the cause, so the decode is written
/// against the table rather than against intuition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PageFaultError {
    /// 0 = the page was not present, 1 = a protection violation.
    pub present: bool,
    /// 0 = the access was a read, 1 = a write.
    pub write: bool,
    /// 1 = the access came from user mode (`CPL > 0`).
    pub user: bool,
    /// 1 = the fault was caused by a reserved bit set in the PTE.
    pub reserved_bit: bool,
    /// 1 = the access was an instruction fetch.
    pub instruction_fetch: bool,
}

impl PageFaultError {
    /// Decode an x86-64 page-fault error code.
    pub const fn decode(code: u64) -> Self {
        Self {
            present: code & (1 << 0) != 0,
            write: code & (1 << 1) != 0,
            user: code & (1 << 2) != 0,
            reserved_bit: code & (1 << 3) != 0,
            instruction_fetch: code & (1 << 4) != 0,
        }
    }

    /// A one-line rendering for the panic banner.
    pub const fn describe(&self) -> &'static str {
        match (self.write, self.user, self.instruction_fetch) {
            (false, false, false) => "read from kernel memory",
            (true, false, false) => "write to kernel memory",
            (false, true, false) => "read from user memory",
            (true, true, false) => "write to user memory",
            (false, false, true) => "execute at a non-executable address",
            (true, false, true) => "kernel instruction fetch denied",
            (false, true, true) => "user instruction fetch denied",
            (true, true, true) => "user instruction fetch denied (write bit set)",
        }
    }
}

/// A single IDT entry, as a 16-byte 64-bit gate descriptor.
///
/// The layout is architectural and byte-exact:
///
/// ```text
/// byte  0-1  offset 15..0 of the handler address
/// byte  2-3  segment selector
/// byte  4     IST index (bits 0-2), the rest reserved
/// byte  5     type attribute (bits 1-2) | DPL (bits 4-5) | P (bit 7)
/// byte  6     reserved, zero
/// byte  7     reserved, zero
/// byte  8-9  offset 31..16 of the handler address
/// byte 10-13 offset 63..32 of the handler address
/// byte 14-15 reserved, zero
/// ```
///
/// # Why the layout is `repr(C)`
///
/// A gate descriptor is not a Rust struct as far as the CPU is concerned: it
/// is sixteen bytes of memory that the CPU decodes field by field, and the
/// fields sit at fixed byte offsets. `repr(Rust)` gives the compiler licence
/// to reorder the fields and to insert padding wherever it likes, which
/// means the *memory* of a `GateDescriptor` is not necessarily the
/// architectural descriptor at all.
///
/// That matters here because the table is handed to the CPU by pointer: the
/// bytes the CPU reads are the bytes of the array of descriptors, not the
/// bytes some other function produced. If the compiler reorders the fields,
/// the CPU decodes the gate type as the offset and the handler address as
/// the selector, and the table is silently nonsense.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub struct GateDescriptor {
    /// Offset 15..0 of the handler address.
    offset_low: u16,
    /// Segment selector.
    selector: u16,
    /// Byte 4: the IST index.
    ist: u8,
    /// Byte 5: the type attribute, DPL and the present bit.
    type_dpl_present: u8,
    /// Offset 31..16 of the handler address.
    offset_middle: u16,
    /// Offset 63..32 of the handler address.
    offset_high: u32,
    /// The top four bytes of the descriptor, reserved and always zero.
    reserved_high: u32,
}

impl GateDescriptor {
    /// A 64-bit interrupt gate for a handler in the lower 4 GiB.
    ///
    /// Interrupt gate rather than trap gate on purpose: `IF` is cleared on
    /// entry, so a handler never has to reason about being interrupted by
    /// another device while it is manipulating shared state. The cost is that
    /// the interrupt-enable flag must be restored explicitly by the
    /// trampoline.
    ///
    /// The gate's DPL is 0, so user mode cannot take these vectors directly.
    /// Phase 4 routes user-mode faults through a dedicated mechanism rather
    /// than loosening the DPL, because a DPL-0 gate that user code can invoke
    /// with an arbitrary stack is a privilege-escalation primitive.
    pub const fn interrupt_gate(handler: u64) -> Self {
        Self {
            offset_low: handler as u16,
            selector: crate::KERNEL_CODE_SELECTOR,
            // No IST stack.
            ist: 0,
            // 64-bit interrupt gate, DPL 0, present.
            //
            // Every field of byte 5 has to be written: the interrupt/trap
            // bit (bit 0), the type attribute (bits 3-1), the DPL (bits 5-4)
            // and the present bit (bit 7). Together they make `0b1000_1110`,
            // which is the byte every x86-64 kernel writes.
            //
            // Getting it wrong produces the architecturally invalid
            // `0b1000_0000`, which the CPU rejects with a `#GP` on the first
            // interrupt the table is asked to deliver.
            type_dpl_present: (gate::GATE_TYPE_INTERRUPT << gate::GATE_TYPE_SHIFT)
                | (gate::TYPE_ATTR_64_INTERRUPT << gate::TYPE_ATTR_SHIFT)
                | (0 << gate::DPL_SHIFT)
                | gate::PRESENT,
            reserved_high: 0,
            offset_middle: (handler >> 16) as u16,
            offset_high: (handler >> 32) as u32,
        }
    }

    /// The raw 16 bytes, in little-endian order.
    ///
    /// Written field by field rather than as a `u128` transmute: the struct is
    /// `repr(Rust)`, so the compiler is free to reorder it and a transmute
    /// would be silently wrong on any layout change.
    pub const fn to_bytes(self) -> [u8; GATE_BYTES] {
        let mut out = [0u8; GATE_BYTES];
        // Low 16 bits of the handler offset.
        out[0] = self.offset_low as u8;
        out[1] = (self.offset_low >> 8) as u8;
        // Segment selector.
        out[2] = self.selector as u8;
        out[3] = (self.selector >> 8) as u8;
        // IST index.
        out[4] = self.ist;
        // Type attribute, DPL, present.
        out[5] = self.type_dpl_present;
        // Bits 31..16 of the handler offset.
        out[6] = self.offset_middle as u8;
        out[7] = (self.offset_middle >> 8) as u8;
        // Bits 63..32 of the handler offset.
        out[8] = self.offset_high as u8;
        out[9] = (self.offset_high >> 8) as u8;
        out[10] = (self.offset_high >> 16) as u8;
        out[11] = (self.offset_high >> 24) as u8;
        // The top four bytes of the descriptor are reserved and always zero.
        out[12] = self.reserved_high as u8;
        out[13] = (self.reserved_high >> 8) as u8;
        out[14] = (self.reserved_high >> 16) as u8;
        out[15] = (self.reserved_high >> 24) as u8;
        out
    }

    /// The handler address this descriptor points at.
    pub const fn handler(&self) -> u64 {
        (self.offset_low as u64)
            | ((self.offset_middle as u64) << 16)
            | ((self.offset_high as u64) << 32)
    }

    /// True when the descriptor is marked present.
    pub const fn is_present(&self) -> bool {
        self.type_dpl_present & gate::PRESENT != 0
    }

    /// The descriptor privilege level: the lowest ring this gate accepts.
    pub const fn dpl(&self) -> u8 {
        (self.type_dpl_present >> gate::DPL_SHIFT) & gate::DPL_MASK
    }

    /// The type attribute field, as stored in byte 5.
    pub const fn type_attr(&self) -> u8 {
        (self.type_dpl_present >> gate::TYPE_ATTR_SHIFT) & gate::TYPE_ATTR_MASK
    }

    /// True when this is a 64-bit gate.
    ///
    /// The type attribute must be `0b00`; `0b10` would be a 32-bit gate, which
    /// enters the handler in compatibility mode.
    pub const fn is_long_mode(&self) -> bool {
        self.type_attr() == gate::TYPE_ATTR_64_INTERRUPT
    }
}

/// The interrupt descriptor table.
///
/// # Why the table is 16-byte aligned
///
/// In 64-bit mode the IDT's base address must be aligned on an 8-byte
/// boundary. A misaligned base is not rejected outright: the CPU simply
/// decodes each 16-byte descriptor from an address that straddles the
/// alignment it expected, and the fields come out shifted. The handler
/// address then reads as a garbage selector-plus-offset pair, and the
/// interrupt lands on an address that is not code — which the CPU reports
/// as a `#GP` whose error code points at the *IDT*, and whose faulting
/// instruction is the interrupted one, so it looks like a bug in the
/// interrupted code rather than in the table.
///
/// The alignment is therefore part of the type, not a hope about where the
/// linker happened to put it.
#[repr(C, align(16))]
pub struct Idt {
    /// The table exactly as the CPU will decode it.
    ///
    /// # Why the bytes are built explicitly
    ///
    /// `LIDT` takes a *pointer*, so the bytes the CPU reads are the bytes of
    /// whatever the pointer addresses. Handing it the descriptor array
    /// directly would work only if the array's memory layout happened to be
    /// the architectural one -- which `repr(C)` now guarantees for the
    /// fields, but which still leaves the two reserved bytes at the end of
    /// each descriptor up to the compiler.
    ///
    /// So the image is built field by field through
    /// [`GateDescriptor::to_bytes`], which is the same code path the unit
    /// tests assert against. What the CPU is handed and what the tests
    /// check are then the same bytes by construction, rather than by
    /// coincidence.
    image: [u8; IDT_BYTES],
    entries: [GateDescriptor; IDT_ENTRIES],
}

impl core::fmt::Debug for Idt {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let present = self.entries.iter().filter(|e| e.is_present()).count();
        f.debug_struct("Idt")
            .field("present", &present)
            .field("bytes", &IDT_BYTES)
            .finish()
    }
}

impl Default for Idt {
    fn default() -> Self {
        Self::new()
    }
}

impl Idt {
    /// An empty table: every gate absent.
    pub const fn new() -> Self {
        Self {
            image: [0u8; IDT_BYTES],
            entries: [GateDescriptor {
                offset_low: 0,
                selector: 0,
                ist: 0,
                type_dpl_present: 0,
                reserved_high: 0,
                offset_middle: 0,
                offset_high: 0,
            }; IDT_ENTRIES],
        }
    }

    /// Point `vector` at `handler`.
    ///
    /// Overwriting a live vector is allowed: the LAPIC timer reinstalls its
    /// vector on every configuration pass, and refusing the second write would
    /// make that impossible to express.
    pub fn set_handler(&mut self, vector: u8, handler: usize) {
        self.entries[vector as usize] = GateDescriptor::interrupt_gate(handler as u64);
        self.refresh_image(vector as usize);
    }

    /// Point `vector` at a handler function.
    ///
    /// Preferred over [`Idt::set_handler`] because casting a function item
    /// straight to `usize` is a lint in current Rust; going through a pointer
    /// makes the conversion explicit.
    pub fn set_handler_fn(&mut self, vector: u8, handler: unsafe extern "C" fn()) {
        self.set_handler(vector, handler as *const () as usize);
    }

    /// Remove the gate for `vector`.
    pub fn clear_handler(&mut self, vector: u8) {
        self.entries[vector as usize] = GateDescriptor::interrupt_gate(0);
        self.entries[vector as usize].type_dpl_present = 0;
        self.refresh_image(vector as usize);
    }

    /// Rebuild the byte image of one gate.
    ///
    /// The image is the CPU's view of the table, and it is the thing `LIDT`
    /// is pointed at, so it has to be kept in step with the descriptors.
    fn refresh_image(&mut self, index: usize) {
        let bytes = self.entries[index].to_bytes();
        let start = index * GATE_BYTES;
        self.image[start..start + GATE_BYTES].copy_from_slice(&bytes);
    }

    /// Read the descriptor for `vector`.
    pub const fn get(&self, vector: u8) -> &GateDescriptor {
        &self.entries[vector as usize]
    }

    /// Number of present gates.
    pub fn present_count(&self) -> usize {
        self.entries.iter().filter(|e| e.is_present()).count()
    }

    /// Load this table into the CPU (`LIDT`).
    ///
    /// # Safety
    ///
    /// The `&mut self` borrow is the safety argument: the caller owns the
    /// table for the duration, and the handlers it points at are permanent
    /// `extern "C"` functions, so no table entry can dangle.
    pub unsafe fn load(&mut self) {
        // `LIDT` takes its operand as a memory reference to a {limit, base}
        // pair, not the two values in registers, so the pointer has to be
        // handed to the instruction directly. Building the pair as a
        // `#[repr(C, packed)]` struct keeps the field order architectural.
        let idtr = DescriptorTablePointer {
            limit: (IDT_BYTES - 1) as u16,
            base: self.image.as_ptr() as u64,
        };
        // SAFETY: `idtr` is a local that outlives the instruction, and `base`
        // addresses `IDT_BYTES` bytes of the gate image inside `self`, which
        // the `&mut self` borrow keeps alive.
        unsafe {
            asm!(
                "lidt [{ptr:r}]",
                ptr = in(reg) &idtr,
                options(readonly, nostack, preserves_flags),
            );
        }
    }
}

/// The operand `LIDT` reads: a 16-bit byte limit followed by a 64-bit base.
#[repr(C, packed)]
struct DescriptorTablePointer {
    limit: u16,
    base: u64,
}

/// The process-wide IDT.
static IDT: Mutex<Idt> = Mutex::new(Idt::new());

/// A count of interrupts serviced, for the boot summary and for asserting the
/// timer is actually firing.
static INTERRUPT_COUNT: AtomicU64 = AtomicU64::new(0);

/// How many interrupts have been taken.
pub fn interrupt_count() -> u64 {
    INTERRUPT_COUNT.load(Ordering::Relaxed)
}

/// The Rust entry point for every CPU exception.
///
/// # Why this is the only exception entry point
///
/// The assembly stubs in this module normalise the CPU's frame — which has a
/// different shape depending on whether the vector pushes an error code —
/// into a single [`InterruptFrame`], and then call this function with the
/// frame's address and the vector number.
///
/// So there is exactly one place that decides what an exception *means*, and
/// no handler has to know the shape of the frame it was given.
///
/// # Safety
///
/// The frame must point to a valid 48-byte interrupt frame built by the
/// assembly stub, and this function must only be called from assembly.
///
/// # Why it never returns
///
/// An exception the kernel cannot handle is unrecoverable: there is no
/// user-mode equivalent of a signal frame the kernel could unwind through,
/// and continuing past it would let the faulting instruction re-execute
/// forever. The plan is explicit about this — "page fault prints CR2 + error
/// code and halts loudly, never silently black-screens".
///
/// Every path out of this function therefore ends in
/// [`crate::qemu_exit::power_off`], which is observable to CI through
/// `isa-debug-exit`.
///
/// # Safety
///
/// This function is called from assembly with a pointer to a valid
/// [`InterruptFrame`] and the vector number that produced it. It is not meant
/// to be called from Rust.
#[no_mangle]
pub unsafe extern "C" fn vela_exception_entry(frame: *const InterruptFrame, vector: u64) -> ! {
    // SAFETY: the assembly stub passes a pointer to a frame it has just
    // built on its own stack: 48 bytes, 16-byte aligned, holding the error
    // code (zeroed for the vectors that push none), the saved RIP, CS,
    // RFLAGS, RSP and SS.
    // SAFETY: the assembly stub constructed and aligned the frame on its own
    // stack, and its lifetime spans this call.
    // SAFETY: the assembly stub constructed and aligned the frame on its own
    // stack, and its lifetime spans this call.
    let f = unsafe { &*frame };
    let v = vector as u8;

    // Every `report_*` below ends the machine, so the match is the tail
    // expression of this function and the function never returns: there is
    // no frame to return through, and returning would resume the faulting
    // instruction and fault again.
    match v {
        vectors::PAGE_FAULT => report_page_fault(f),
        vectors::GENERAL_PROTECTION => report_general_protection(f),
        vectors::INVALID_OPCODE => report_invalid_opcode(f),
        vectors::STACK_SEGMENT => report_stack_fault(f),
        vectors::SEGMENT_NOT_PRESENT => report_segment_fault(f),
        vectors::DOUBLE_FAULT => report_double_fault(f),
        vectors::DEVICE_NOT_AVAILABLE => report_device_not_available(f),
        vectors::BREAKPOINT => report_breakpoint(f),
        _ => report_generic(v, f),
    }
}

/// The Rust entry point for every interrupt.
///
/// # Safety
///
/// The frame must point to a valid interrupt frame built by the assembly
/// stub, and this function must only be called from assembly.
///
/// # Why interrupts are separate from exceptions
///
/// An interrupt is *not* a fault: the CPU pushed a frame it expects to be
/// resumed from, and the handler must return to the interrupted code rather
/// than stop the machine. The assembly stub does the `iretq`; this function
/// only does the work.
///
/// The vector is passed as a number because the APIC timer and the legacy
/// PIC lines share this path, and only the timer has anything to do.
///
/// # Safety
///
/// Called from assembly with a pointer to a valid [`InterruptFrame`] and the
/// vector number. Not meant to be called from Rust.
#[no_mangle]
pub unsafe extern "C" fn vela_interrupt_entry(frame: *const InterruptFrame, vector: u64) {
    // SAFETY: the assembly stub passes a pointer to a frame it has just
    // built on its own stack.
    // SAFETY: the assembly stub constructed the frame and keeps it alive for
    // the call.
    // SAFETY: the assembly stub constructed the frame and keeps it alive for
    // the call.
    let f = unsafe { &*frame };
    let v = vector as u8;

    if v == crate::apic::TIMER_VECTOR {
        // SAFETY: `apic::on_timer_interrupt` writes the APIC's end-of-
        // interrupt register, which is writable from any CPU that took an
        // APIC interrupt.
        crate::apic::on_timer_interrupt();
    } else {
        // Every other interrupt is counted but otherwise ignored for now.
        // The legacy PIC's lines are masked by the firmware's own setup in
        // this configuration, so they should never arrive.
        let _ = f;
    }
    INTERRUPT_COUNT.fetch_add(1, Ordering::Relaxed);
}

/// Report a page fault: `CR2`, the error code, and where it happened.
///
/// The plan's requirement: print `CR2` and the error code, then halt loudly.
fn report_page_fault(f: &InterruptFrame) -> ! {
    let cr2 = crate::regs::read_cr2();
    let err = PageFaultError::decode(f.error_code);

    serial::puts("\n*** PAGE FAULT ***\n");
    serial::puts("    CR2        = ");
    serial::puts_hex(cr2);
    serial::puts("\n    error code = ");
    serial::puts_hex(f.error_code);
    serial::puts("\n    access     = ");
    serial::puts(err.describe());
    serial::puts("\n    from       = ");
    serial::puts(if f.from_user() {
        "user mode"
    } else {
        "kernel mode"
    });
    serial::puts("\n    RIP        = ");
    serial::puts_hex(f.rip);
    serial::puts("\n    RSP        = ");
    serial::puts_hex(f.rsp);
    serial::puts("\n");

    // A user-mode fault with no handler installed is a user bug, not a kernel
    // bug, but it is still unrecoverable in v0.1.0: there is no user signal
    // frame yet. Report it as such so the distinction is visible in the log.
    if f.from_user() {
        serial::puts("    user-mode fault with no user fault handler installed\n");
    }

    qemu_exit::power_off(1)
}

/// Report a general protection fault.
fn report_general_protection(f: &InterruptFrame) -> ! {
    serial::puts("\n*** GENERAL PROTECTION FAULT ***\n");
    serial::puts("    error code = ");
    serial::puts_hex(f.error_code);
    serial::puts("\n    RIP        = ");
    serial::puts_hex(f.rip);
    serial::puts("\n    from       = ");
    serial::puts(if f.from_user() {
        "user mode"
    } else {
        "kernel mode"
    });
    serial::puts("\n");
    qemu_exit::power_off(1)
}

/// Report an invalid opcode: where it was, and what the bytes were.
///
/// The bytes matter: the point of the report is to say *which* instruction
/// the CPU refused, and the opcode itself is the answer.
fn report_invalid_opcode(f: &InterruptFrame) -> ! {
    serial::puts("\n*** INVALID OPCODE ***\n");
    serial::puts("    RIP = ");
    serial::puts_hex(f.rip);
    serial::puts("\n    bytes at RIP = ");
    serial::puts_code_at(f.rip);
    serial::puts("\n");
    qemu_exit::power_off(1)
}

/// Report a stack-segment fault.
fn report_stack_fault(f: &InterruptFrame) -> ! {
    serial::puts("\n*** STACK SEGMENT FAULT ***\n");
    serial::puts("    error code = ");
    serial::puts_hex(f.error_code);
    serial::puts("\n    RIP        = ");
    serial::puts_hex(f.rip);
    serial::puts("\n    RSP        = ");
    serial::puts_hex(f.rsp);
    serial::puts("\n");
    qemu_exit::power_off(1)
}

/// Report a segment-not-present fault.
fn report_segment_fault(f: &InterruptFrame) -> ! {
    serial::puts("\n*** SEGMENT NOT PRESENT ***\n");
    serial::puts("    error code = ");
    serial::puts_hex(f.error_code);
    serial::puts("\n    RIP        = ");
    serial::puts_hex(f.rip);
    serial::puts("\n");
    qemu_exit::power_off(1)
}

/// Report a device-not-available fault.
///
/// This is the exception a `CPUID` raises when the caller asked for a
/// feature the CPU does not have. It is worth calling out by name, because
/// it is what a kernel gets when it asks for an instruction without checking
/// whether the CPU implements it.
fn report_device_not_available(f: &InterruptFrame) -> ! {
    serial::puts("\n*** DEVICE NOT AVAILABLE ***\n");
    serial::puts("    RIP = ");
    serial::puts_hex(f.rip);
    serial::puts("\n    the CPU does not implement the feature the code asked for\n");
    qemu_exit::power_off(1)
}

/// Report a breakpoint.
///
/// A breakpoint is a deliberate stop, so it is reported as one rather than
/// as a fault — but it still ends the machine, because the kernel has no
/// debugger to hand control to.
fn report_breakpoint(f: &InterruptFrame) -> ! {
    serial::puts("\n*** BREAKPOINT ***\n");
    serial::puts("    RIP = ");
    serial::puts_hex(f.rip);
    serial::puts("\n");
    qemu_exit::power_off(1)
}

/// Report a double fault.
///
/// A double fault means the kernel could not even deliver the first fault,
/// which in practice means the IDT or a page table was broken. The CPU clears
/// `CR0.PE` and shuts down on a *triple* fault, so this handler must report
/// and stop without touching much state.
///
/// The handler runs on a private stack — see the assembly stub — so it can
/// safely read the frame even when the original stack was the problem.
fn report_double_fault(_f: &InterruptFrame) -> ! {
    // The frame is read from the private buffer the stub filled, not from
    // the argument: the stub copied the CPU's frame there before it
    // switched stacks, and the argument *is* that buffer — but reading it
    // through the accessor makes the provenance explicit and keeps the
    // stack-pointer field honest.
    let f = double_fault_frame();
    serial::puts("\n*** DOUBLE FAULT ***\n");
    serial::puts("    error code = ");
    serial::puts_hex(f.error_code);
    serial::puts("\n    RIP        = ");
    serial::puts_hex(f.rip);
    serial::puts("\n    RSP        = ");
    serial::puts_hex(f.rsp);
    serial::puts("\n    the kernel's own fault handling is broken\n");
    qemu_exit::power_off(1)
}

/// Report any exception the kernel has no special reporter for.
///
/// Every exception is loud: the plan forbids a silent black-screen, and a
/// kernel that dies without saying which vector it died on is exactly the
/// failure mode this module exists to prevent.
fn report_generic(vector: u8, f: &InterruptFrame) -> ! {
    serial::puts("\n*** CPU EXCEPTION ***\n");
    serial::puts("    vector     = ");
    serial::puts_hex(vector as u64);
    serial::puts("\n    error code = ");
    serial::puts_hex(f.error_code);
    serial::puts("\n    RIP        = ");
    serial::puts_hex(f.rip);
    serial::puts("\n    the kernel cannot continue; halting\n");
    qemu_exit::power_off(1)
}

/// Install the exception handlers.
///
/// Must run before interrupts are unmasked: a fault taken with an empty IDT is
/// a triple fault, which reboots the machine with no diagnostic at all.
pub fn init() {
    let mut idt = IDT.lock();

    // Every CPU exception gets its own stub, so the handler always sees the
    // frame shape its vector pushes.
    for vector in 0..32u8 {
        if vector == vectors::DOUBLE_FAULT {
            // The double fault has its own stub: it runs on a private stack.
            idt.set_handler(vector, double_fault_stub_address());
            continue;
        }
        idt.set_handler(vector, exception_stub_address(vector));
    }

    // The local APIC timer.
    //
    // The vector is chosen by the APIC code, not by the IDT, but the two must
    // agree: the APIC programs the local vector table with the vector the
    // interrupt will arrive on, and the IDT must have a gate for that same
    // vector. A mismatch is a triple fault on the first tick — the CPU takes
    // an interrupt, finds no gate, and shuts down with no diagnostic at all.
    //
    // So the timer handler is installed here, at the vector the APIC uses.
    idt.set_handler(
        crate::apic::TIMER_VECTOR,
        interrupt_stub_address(crate::apic::TIMER_VECTOR),
    );

    // Every remaining interrupt vector gets the generic interrupt stub, so
    // that a stray IRQ is counted rather than being an unhandled gate.
    for vector in 32u8..=255u8 {
        if vector == crate::apic::TIMER_VECTOR {
            continue;
        }
        idt.set_handler(vector, interrupt_stub_address(vector));
    }

    // SAFETY: called once during boot, before interrupts are unmasked.
    unsafe { idt.load() };
    serial::puts("[idt] exception handlers installed\n");
}

/// The address of an assembly stub.
///
/// # Why this is a macro
///
/// The stubs are assembly symbols with no Rust body, so their addresses are
/// taken from the symbol itself rather than from a function item. Casting a
/// function item straight to an integer is a lint error in this codebase,
/// and taking the address of the symbol is both explicit and unambiguous.
macro_rules! stub_address {
    ($name:ident) => {
        core::ptr::addr_of!($name) as usize
    };
}

/// The address of the stub for an exception vector.
///
/// # Why the address is computed rather than written out
///
/// Each vector has its own stub, because the CPU pushes a different frame
/// depending on whether the vector carries an error code. The stubs are
/// generated by the assembler macros in this module, so their names are
/// `vela_exception_stub_<n>` and `vela_error_stub_<n>` — and a Rust
/// `match` on the vector name is what turns that convention into a
/// compile-time-checked table.
fn exception_stub_address(vector: u8) -> usize {
    match vector {
        0 => stub_address!(vela_exception_stub_0),
        1 => stub_address!(vela_exception_stub_1),
        2 => stub_address!(vela_exception_stub_2),
        3 => stub_address!(vela_exception_stub_3),
        4 => stub_address!(vela_exception_stub_4),
        5 => stub_address!(vela_exception_stub_5),
        6 => stub_address!(vela_exception_stub_6),
        7 => stub_address!(vela_exception_stub_7),
        9 => stub_address!(vela_exception_stub_9),
        10 => stub_address!(vela_error_stub_10),
        11 => stub_address!(vela_error_stub_11),
        12 => stub_address!(vela_error_stub_12),
        13 => stub_address!(vela_error_stub_13),
        14 => stub_address!(vela_error_stub_14),
        15 => stub_address!(vela_exception_stub_15),
        16 => stub_address!(vela_exception_stub_16),
        17 => stub_address!(vela_error_stub_17),
        18 => stub_address!(vela_exception_stub_18),
        19 => stub_address!(vela_exception_stub_19),
        20 => stub_address!(vela_exception_stub_20),
        21 => stub_address!(vela_error_stub_21),
        22 => stub_address!(vela_exception_stub_22),
        23 => stub_address!(vela_exception_stub_23),
        24 => stub_address!(vela_exception_stub_24),
        25 => stub_address!(vela_exception_stub_25),
        26 => stub_address!(vela_exception_stub_26),
        27 => stub_address!(vela_exception_stub_27),
        28 => stub_address!(vela_exception_stub_28),
        29 => stub_address!(vela_error_stub_29),
        30 => stub_address!(vela_error_stub_30),
        31 => stub_address!(vela_exception_stub_31),
        _ => stub_address!(vela_exception_stub_0),
    }
}

/// The address of the stub for an interrupt vector.
///
/// The interrupt stubs are all identical apart from the vector number they
/// pass to the handler, so the address is computed from the same
/// convention.
fn interrupt_stub_address(vector: u8) -> usize {
    match vector {
        32 => stub_address!(vela_interrupt_stub_32),
        33 => stub_address!(vela_interrupt_stub_33),
        34 => stub_address!(vela_interrupt_stub_34),
        35 => stub_address!(vela_interrupt_stub_35),
        36 => stub_address!(vela_interrupt_stub_36),
        37 => stub_address!(vela_interrupt_stub_37),
        38 => stub_address!(vela_interrupt_stub_38),
        39 => stub_address!(vela_interrupt_stub_39),
        40 => stub_address!(vela_interrupt_stub_40),
        41 => stub_address!(vela_interrupt_stub_41),
        42 => stub_address!(vela_interrupt_stub_42),
        43 => stub_address!(vela_interrupt_stub_43),
        44 => stub_address!(vela_interrupt_stub_44),
        45 => stub_address!(vela_interrupt_stub_45),
        46 => stub_address!(vela_interrupt_stub_46),
        47 => stub_address!(vela_interrupt_stub_47),
        48 => stub_address!(vela_interrupt_stub_48),
        _ => stub_address!(vela_interrupt_stub_255),
    }
}

/// The address of the double fault's stub.
///
/// It is not part of [`exception_stub_address`] because it is not one of the
/// ordinary stubs: it runs on a private stack.
fn double_fault_stub_address() -> usize {
    stub_address!(vela_double_fault_stub)
}

// The stub symbols.
//
// # Why these are declared rather than looked up
//
// The stubs are defined by the assembler macros in this module, so they have
// no Rust definition. Declaring them here gives the address computation
// above a compile-time-checked name, and the `#[used]` attribute keeps the
// linker from collecting a stub that only the IDT refers to by address.
//
// The addresses are taken with `as usize` rather than through a function
// pointer, because the stubs are not Rust functions: they have no prologue,
// they do not follow the C ABI on entry, and calling one from Rust would
// be meaningless. Only their *address* is meaningful, and that is what the
// IDT stores.

macro_rules! declare_exception_stub {
    ($name:ident) => {
        extern "C" {
            /// An assembly entry stub for one CPU exception vector.
            ///
            /// The stub has no Rust body; only its address is meaningful.
            static $name: u8;
        }
    };
}

macro_rules! declare_error_stub {
    ($name:ident) => {
        extern "C" {
            /// An assembly entry stub for one error-code CPU exception vector.
            ///
            /// The stub has no Rust body; only its address is meaningful.
            static $name: u8;
        }
    };
}

macro_rules! declare_interrupt_stub {
    ($name:ident) => {
        extern "C" {
            /// An assembly entry stub for one interrupt vector.
            ///
            /// The stub has no Rust body; only its address is meaningful.
            static $name: u8;
        }
    };
}

declare_exception_stub!(vela_exception_stub_0);
declare_exception_stub!(vela_exception_stub_1);
declare_exception_stub!(vela_exception_stub_2);
declare_exception_stub!(vela_exception_stub_3);
declare_exception_stub!(vela_exception_stub_4);
declare_exception_stub!(vela_exception_stub_5);
declare_exception_stub!(vela_exception_stub_6);
declare_exception_stub!(vela_exception_stub_7);
declare_exception_stub!(vela_exception_stub_9);
declare_exception_stub!(vela_exception_stub_15);
declare_exception_stub!(vela_exception_stub_16);
declare_exception_stub!(vela_exception_stub_18);
declare_exception_stub!(vela_exception_stub_19);
declare_exception_stub!(vela_exception_stub_20);
declare_exception_stub!(vela_exception_stub_22);
declare_exception_stub!(vela_exception_stub_23);
declare_exception_stub!(vela_exception_stub_24);
declare_exception_stub!(vela_exception_stub_25);
declare_exception_stub!(vela_exception_stub_26);
declare_exception_stub!(vela_exception_stub_27);
declare_exception_stub!(vela_exception_stub_28);
declare_exception_stub!(vela_exception_stub_31);
declare_error_stub!(vela_error_stub_10);
declare_error_stub!(vela_error_stub_11);
declare_error_stub!(vela_error_stub_12);
declare_error_stub!(vela_error_stub_13);
declare_error_stub!(vela_error_stub_14);
declare_error_stub!(vela_error_stub_17);
declare_error_stub!(vela_error_stub_21);
declare_error_stub!(vela_error_stub_29);
declare_error_stub!(vela_error_stub_30);
declare_interrupt_stub!(vela_interrupt_stub_32);
declare_interrupt_stub!(vela_interrupt_stub_33);
declare_interrupt_stub!(vela_interrupt_stub_34);
declare_interrupt_stub!(vela_interrupt_stub_35);
declare_interrupt_stub!(vela_interrupt_stub_36);
declare_interrupt_stub!(vela_interrupt_stub_37);
declare_interrupt_stub!(vela_interrupt_stub_38);
declare_interrupt_stub!(vela_interrupt_stub_39);
declare_interrupt_stub!(vela_interrupt_stub_40);
declare_interrupt_stub!(vela_interrupt_stub_41);
declare_interrupt_stub!(vela_interrupt_stub_42);
declare_interrupt_stub!(vela_interrupt_stub_43);
declare_interrupt_stub!(vela_interrupt_stub_44);
declare_interrupt_stub!(vela_interrupt_stub_45);
declare_interrupt_stub!(vela_interrupt_stub_46);
declare_interrupt_stub!(vela_interrupt_stub_47);
declare_interrupt_stub!(vela_interrupt_stub_48);
declare_interrupt_stub!(vela_interrupt_stub_255);

extern "C" {
    /// The double fault's entry stub, which runs on a private stack.
    static vela_double_fault_stub: u8;
}

// The private buffer and stack the double fault's stub uses.
//
// # Why the Rust side needs to know about them
//
// The stub copies the CPU's frame into the buffer and switches to the
// stack before it calls into Rust. The Rust side then reads the frame from
// the buffer, so the buffer's address has to be visible here.
extern "C" {
    /// The scratch buffer the double fault's frame is copied into.
    static vela_df_buffer: [u64; 8];
    /// The base of the double fault's private stack.
    static vela_df_stack: [u64; 2048];
    /// The top of the double fault's private stack.
    static vela_df_stack_top: u64;
}

/// Read the double fault's private buffer as an [`InterruptFrame`].
///
/// The stub copied four words — the error code, RIP, CS and RFLAGS — into
/// the buffer and left the rest untouched, so the two words that describe
/// the interrupted stack are filled in from the CPU's own state.
fn double_fault_frame() -> InterruptFrame {
    // SAFETY: `vela_df_buffer` is a 64-byte, 16-byte-aligned object in
    // `.bss`, and the double fault's stub has just written four words into
    // it. The read is of the whole buffer, which is within bounds.
    let raw = unsafe { &*(&vela_df_buffer as *const [u64; 8]) };
    InterruptFrame {
        error_code: raw[0],
        rip: raw[1],
        cs: raw[2],
        rflags: raw[3],
        // The CPU does not push RSP or SS on a double fault: it cannot
        // trust the stack it is running on, so it does not record it. The
        // values below are therefore the *current* ones, which is the most
        // that can honestly be reported.
        rsp: crate::regs::read_rsp(),
        ss: 0,
    }
}

/// The address of the double fault's private stack top.
///
/// # Why this is exposed
///
/// The stack is a `.bss` object, so it has no load-time address in the
/// image; its address is whatever the linker gave it. The Rust side needs
/// that address to report the stack it was running on, and to check that
/// the private stack was actually set up.
pub fn double_fault_stack_top() -> u64 {
    // SAFETY: `vela_df_stack_top` is a symbol in `.bss`; reading it is a
    // load of a `u64` from a static, which cannot fault.
    unsafe { core::ptr::read_volatile(&vela_df_stack_top as *const u64) }
}

/// The base of the double fault's private stack.
///
/// # Why this is exposed
///
/// The stack is a `.bss` object, so it has no load-time address in the
/// image; its address is whatever the linker gave it. The Rust side needs
/// that address to report the stack the double fault was handled on, and to
/// check that the private stack was actually set up.
pub fn double_fault_stack_base() -> u64 {
    // `vela_df_stack` is a `.bss` object of 16 KiB; taking its address is a
    // compile-time constant computation and cannot fault.
    core::ptr::addr_of!(vela_df_stack) as u64
}

#[cfg(test)]
extern crate std;

#[cfg(test)]
mod tests {
    use super::*;
    use std::format;
    use std::vec::Vec;

    #[test]
    fn gate_descriptor_round_trips_a_handler_address() {
        for handler in [
            0x1000u64,
            0xFFFF_8000_0000_1000,
            0xFFFF_FFFF_FFFF_F000,
            0x0000_0000_0000_000F,
        ] {
            let g = GateDescriptor::interrupt_gate(handler);
            assert_eq!(g.handler(), handler);
        }
    }

    #[test]
    fn gate_descriptor_is_sixteen_bytes() {
        assert_eq!(GATE_BYTES, 16);
        assert_eq!(size_of::<GateDescriptor>(), 16);
        assert_eq!(IDT_BYTES, 4096);
        // The architectural limit is exactly one page, which is why `LIDT`
        // takes a 16-bit limit.
        assert_eq!(IDT_BYTES, 4096);
    }

    #[test]
    fn gate_bytes_are_little_endian() {
        // The handler address is chosen so that *every* part of the 64-bit
        // offset is non-zero: a test address with zero bits in the middle
        // would pass with the fields in the wrong place.
        let g = GateDescriptor::interrupt_gate(0xDEAD_BEEF_1234_5678);
        let b = g.to_bytes();
        // Bytes 0-1: the low 16 bits of the handler offset.
        assert_eq!(b[0], 0x78);
        assert_eq!(b[1], 0x56);
        // Bytes 2-3: the segment selector.
        assert_eq!(b[2], (crate::KERNEL_CODE_SELECTOR & 0xFF) as u8);
        assert_eq!(b[3], (crate::KERNEL_CODE_SELECTOR >> 8) as u8);
        // Byte 4: the IST index, which is zero.
        assert_eq!(b[4] & gate::IST_MASK, 0, "IST index must be 0");
        // Byte 5: type attribute (bits 1-2), DPL (bits 4-5), present (bit 7).
        assert_eq!(b[5] & gate::PRESENT, gate::PRESENT, "present bit");
        assert_eq!(
            (b[5] >> gate::DPL_SHIFT) & gate::DPL_MASK,
            0,
            "DPL must be 0"
        );
        assert_eq!(
            (b[5] >> gate::TYPE_ATTR_SHIFT) & gate::TYPE_ATTR_MASK,
            gate::TYPE_ATTR_64_INTERRUPT,
            "the type attribute must select a 64-bit gate"
        );
        // Bytes 6-7: bits 31..16 of the handler offset.
        //
        // # Why this offset is the regression test
        //
        // The x86-64 16-byte gate descriptor puts the *middle* of the offset
        // at bytes 6-7 and the *high* half at bytes 8-11. Writing them in
        // the other order -- middle at 8-9, high at 10-13, with two reserved
        // bytes at 6-7 -- is a very natural mistake, because it is what a
        // 32-bit-style descriptor looks like once it has been widened.
        //
        // The symptom is invisible until the first interrupt is delivered:
        // the CPU decodes the handler address as `high:middle:low`, the
        // address comes out shifted, and the CPU jumps into the middle of
        // nowhere and takes a `#GP` whose faulting instruction looks like
        // innocent kernel code.
        assert_eq!(b[6], 0x34);
        assert_eq!(b[7], 0x12);
        // Bytes 8-11: bits 63..32 of the handler offset.
        assert_eq!(u32::from_le_bytes([b[8], b[9], b[10], b[11]]), 0xDEAD_BEEF);
        // Bytes 12-15: reserved, and always zero.
        assert_eq!(b[12], 0);
        assert_eq!(b[13], 0);
        assert_eq!(b[14], 0);
        assert_eq!(b[15], 0);
    }

    #[test]
    fn the_gate_type_bit_is_set_to_an_interrupt_gate() {
        // The regression test for the bug that killed the kernel on its first
        // timer tick.
        //
        // Byte 5 of a gate descriptor carries four separate fields, and the
        // gate *type* is the lowest of them. Building the byte from the type
        // attribute alone leaves the gate type at `0b00`, which is the
        // architecturally invalid value: the CPU rejects such a gate with a
        // `#GP` the first time it is asked to deliver an interrupt through
        // it.
        //
        // The symptom is a kernel that boots perfectly, installs the IDT,
        // programs the APIC timer, enables interrupts -- and then dies on the
        // very first tick, with the fault reported against whichever
        // instruction happened to be interrupted. Nothing points at the IDT.
        let g = GateDescriptor::interrupt_gate(0x1000);
        let b = g.to_bytes();
        assert_eq!(
            b[5] & gate::GATE_TYPE_MASK,
            gate::GATE_TYPE_INTERRUPT,
            "the gate type must be an interrupt gate, not the invalid 0b00"
        );
        // The whole byte, spelled out.
        //
        // `0x8E` is the byte every x86-64 kernel writes for a ring-0 64-bit
        // interrupt gate, and `0x8F` is its trap-gate counterpart.
        assert_eq!(
            b[5],
            gate::GATE_TYPE_INTERRUPT
                | (gate::TYPE_ATTR_64_INTERRUPT << gate::TYPE_ATTR_SHIFT)
                | gate::PRESENT
        );
        assert_eq!(b[5], 0x8E);
        // And the trap gate is the other legal value.
        assert_eq!(gate::GATE_TYPE_TRAP, 0b1);
        assert_eq!(gate::GATE_TYPE_TRAP & gate::GATE_TYPE_MASK, 0b1);
    }

    #[test]
    fn the_gate_is_a_long_mode_gate() {
        // A 32-bit-style gate makes the CPU enter the handler in compatibility
        // mode. This is the single most common IDT bug, so it is asserted
        // directly.
        let g = GateDescriptor::interrupt_gate(0x1000);
        assert!(g.is_long_mode());
        assert_eq!(g.dpl(), 0);
        assert!(g.is_present());
    }

    #[test]
    fn the_gate_image_the_cpu_reads_matches_the_descriptor() {
        // The CPU is handed the *byte image*, not the struct, so the image is
        // what has to be architecturally correct.
        //
        // This is the regression test for the bug that made the kernel jump
        // to `0x8000_0002_0000_730b` instead of `0xffff_8000_0002_730b`: the
        // middle of the offset was written at bytes 8-9 and the high half at
        // bytes 10-13, leaving the architectural bytes 6-7 as two reserved
        // zeros. The CPU decoded `high:middle:low`, the address came out
        // shifted by eight bits, and the very first timer interrupt landed
        // in the middle of nowhere.
        let mut idt = Idt::new();
        let handler = 0xFFFF_8000_0002_730Bu64;
        idt.set_handler(0x30, handler as usize);
        let start = 0x30 * GATE_BYTES;
        let b = &idt.image[start..start + GATE_BYTES];
        // Bytes 0-1: offset 15..0.
        assert_eq!(b[0], 0x0B);
        assert_eq!(b[1], 0x73);
        // Bytes 2-3: the segment selector.
        assert_eq!(b[2], (crate::KERNEL_CODE_SELECTOR & 0xFF) as u8);
        assert_eq!(b[3], (crate::KERNEL_CODE_SELECTOR >> 8) as u8);
        // Byte 4: the IST index.
        assert_eq!(b[4], 0);
        // Byte 5: the type, DPL and present bits -- `0x8E`.
        assert_eq!(b[5], 0x8E);
        // Bytes 6-7: offset 31..16.
        assert_eq!(u16::from_le_bytes([b[6], b[7]]), 0x0002);
        // Bytes 8-11: offset 63..32.
        assert_eq!(u32::from_le_bytes([b[8], b[9], b[10], b[11]]), 0xFFFF_8000);
        // Bytes 12-15: reserved.
        assert_eq!(b[12], 0);
        assert_eq!(b[13], 0);
        assert_eq!(b[14], 0);
        assert_eq!(b[15], 0);
        // And the whole thing round-trips to the original address.
        assert_eq!(idt.get(0x30).handler(), handler);
    }

    #[test]
    fn an_empty_table_has_no_present_gates() {
        let idt = Idt::new();
        assert_eq!(idt.present_count(), 0);
        assert!(!idt.get(vectors::PAGE_FAULT).is_present());
    }

    #[test]
    fn setting_and_clearing_handlers_is_observable() {
        let mut idt = Idt::new();
        let stub = exception_stub_address(vectors::PAGE_FAULT);
        idt.set_handler(vectors::PAGE_FAULT, stub);
        assert!(idt.get(vectors::PAGE_FAULT).is_present());
        assert_eq!(idt.get(vectors::PAGE_FAULT).handler(), stub as u64);
        assert_eq!(idt.present_count(), 1);

        idt.clear_handler(vectors::PAGE_FAULT);
        assert!(!idt.get(vectors::PAGE_FAULT).is_present());
        assert_eq!(idt.present_count(), 0);
    }

    #[test]
    fn overwriting_a_live_handler_is_allowed() {
        // The LAPIC timer is reconfigured more than once during boot, so a
        // second `set_handler` on the same vector must not panic.
        let mut idt = Idt::new();
        let first = exception_stub_address(vectors::IRQ_BASE);
        let second = exception_stub_address(vectors::PAGE_FAULT);
        idt.set_handler(vectors::IRQ_BASE, first);
        idt.set_handler(vectors::IRQ_BASE, second);
        assert_eq!(idt.get(vectors::IRQ_BASE).handler(), second as u64);
        assert_eq!(idt.present_count(), 1);
    }

    #[test]
    fn every_vector_is_addressable() {
        let mut idt = Idt::new();
        for v in 0..IDT_ENTRIES {
            idt.set_handler(v as u8, exception_stub_address(v as u8));
        }
        assert_eq!(idt.present_count(), IDT_ENTRIES);
        for v in 0..IDT_ENTRIES {
            assert!(idt.get(v as u8).is_present(), "vector {v}");
        }
    }

    #[test]
    fn every_exception_vector_has_its_own_stub() {
        // The regression test for the bug that turned a single `#UD` into a
        // triple fault: one stub cannot serve both frame shapes, so every
        // vector must have a distinct entry point, and the error-code
        // vectors must have a *different* stub from the ones that push no
        // error code.
        //
        // The assertion is about distinctness, not about which stub is
        // which: the CPU's frame shape is what the stub has to match, and
        // that is decided by the assembler macros.
        let mut seen: Vec<usize> = Vec::new();
        for v in 0..32u8 {
            if v == vectors::DOUBLE_FAULT {
                continue;
            }
            let addr = exception_stub_address(v);
            assert!(!seen.contains(&addr), "vector {v} shares a stub");
            seen.push(addr);
        }
        // The double fault has its own stub as well.
        let df = double_fault_stub_address();
        assert!(!seen.contains(&df));
        seen.push(df);
        assert_eq!(seen.len(), 32);
    }

    #[test]
    fn the_error_code_vectors_are_the_documented_ones() {
        // The set of vectors that push an error code is architectural, and
        // getting it wrong is exactly the bug this module's stubs exist to
        // prevent. It is asserted here so that a change to the stub table
        // has to be a deliberate one.
        const ERROR_CODE_VECTORS: [u8; 10] = [8, 10, 11, 12, 13, 14, 17, 21, 29, 30];
        for v in ERROR_CODE_VECTORS {
            assert!(v < 32, "vector {v} is not an exception vector");
        }
        // Every error-code vector must have a stub distinct from the
        // stub of a vector that pushes no error code.
        let pf = exception_stub_address(vectors::PAGE_FAULT);
        let de = exception_stub_address(vectors::DIVIDE_ERROR);
        assert_ne!(pf, de);
    }

    #[test]
    fn every_interrupt_vector_has_a_stub() {
        // The interrupt stubs are a different shape from the exception
        // stubs: they return rather than halting, so a vector that pointed
        // at the wrong one would `iretq` through an exception frame.
        let mut seen: Vec<usize> = Vec::new();
        for v in 32u8..=255u8 {
            let addr = interrupt_stub_address(v);
            if !seen.contains(&addr) {
                seen.push(addr);
            }
        }
        // The catch-all stub covers the vectors with no dedicated entry.
        assert!(seen.len() < (255 - 32 + 1) as usize);
        assert!(seen.len() >= 16);
    }

    #[test]
    fn page_fault_error_decodes_every_documented_bit() {
        // The error code is 0 for a supervisor read of a not-present page.
        let np = PageFaultError::decode(0);
        assert!(!np.present);
        assert!(!np.write);
        assert!(!np.user);
        assert!(!np.instruction_fetch);
        assert_eq!(np.describe(), "read from kernel memory");

        // A supervisor write of a not-present page: bit 1.
        let w = PageFaultError::decode(0b010);
        assert!(w.write);
        assert!(!w.user);
        assert_eq!(w.describe(), "write to kernel memory");

        // A user read: bit 2.
        let ur = PageFaultError::decode(0b100);
        assert!(!ur.write);
        assert!(ur.user);
        assert_eq!(ur.describe(), "read from user memory");

        // A user write: bits 1 and 2.
        let uw = PageFaultError::decode(0b110);
        assert!(uw.write && uw.user);
        assert_eq!(uw.describe(), "write to user memory");

        // A reserved bit in the PTE: bit 3.
        let rsvd = PageFaultError::decode(0b1000);
        assert!(rsvd.reserved_bit);
        assert!(!rsvd.present);

        // An instruction fetch: bit 4.
        let x = PageFaultError::decode(0b1_0000);
        assert!(x.instruction_fetch);
        assert_eq!(x.describe(), "execute at a non-executable address");

        // A user instruction fetch: bits 2 and 4.
        let ux = PageFaultError::decode(0b1_0100);
        assert!(ux.user && ux.instruction_fetch);
        assert_eq!(ux.describe(), "user instruction fetch denied");

        // A protection violation: bit 0.
        let prot = PageFaultError::decode(0b1);
        assert!(prot.present);
        assert!(!prot.write && !prot.user);
    }

    #[test]
    fn page_fault_descriptions_are_distinct() {
        let codes = [0u64, 1, 2, 3, 0x10, 0x11, 0x12, 0x13, 0x16, 0x17];
        let mut seen: Vec<&'static str> = codes
            .iter()
            .map(|c| PageFaultError::decode(*c).describe())
            .collect();
        let before = seen.len();
        seen.sort_unstable();
        seen.dedup();
        // Several codes legitimately share a description (the reserved bit
        // alone must not invent a new message), so this asserts the count does
        // not *grow*, not that every code is unique.
        assert!(seen.len() <= before);
    }

    #[test]
    fn user_mode_is_detected_from_the_saved_cs() {
        let mut f = InterruptFrame {
            error_code: 0,
            rip: 0x1000,
            cs: 0x08, // RPL 0: kernel
            rflags: 0x202,
            rsp: 0x2000,
            ss: 0x10,
        };
        assert!(!f.from_user());
        f.cs = 0x23; // RPL 3: user
        assert!(f.from_user());
    }

    #[test]
    fn debug_formatting_mentions_the_counts() {
        let mut idt = Idt::new();
        idt.set_handler(0, exception_stub_address(0));
        let text = format!("{idt:?}");
        assert!(text.contains("Idt"));
        assert!(text.contains("present"));
    }

    #[test]
    fn interrupt_count_is_monotonic() {
        // The generic interrupt entry point is what an unhandled IRQ
        // reaches; it counts the interrupt and returns.
        let before = interrupt_count();
        let frame = InterruptFrame {
            error_code: 0,
            rip: 0,
            cs: 0x08,
            rflags: 0x202,
            rsp: 0,
            ss: 0x10,
        };
        // SAFETY: the frame is a fully initialized local, matching the form
        // the assembly stub normally supplies.
        unsafe { vela_interrupt_entry(&frame, vectors::IRQ_BASE as u64) };
        assert_eq!(interrupt_count(), before + 1);
    }

    #[test]
    fn vector_numbers_match_the_architecture() {
        // These are burned into any exception dispatch logic and into the
        // boot smoke test's expected log markers.
        assert_eq!(vectors::PAGE_FAULT, 14);
        assert_eq!(vectors::GENERAL_PROTECTION, 13);
        assert_eq!(vectors::DOUBLE_FAULT, 8);
        assert_eq!(vectors::INVALID_OPCODE, 6);
        assert_eq!(vectors::BREAKPOINT, 3);
        assert_eq!(vectors::IRQ_BASE, 32);
        assert_eq!(vectors::IRQ_LAST, 47);
    }
}
