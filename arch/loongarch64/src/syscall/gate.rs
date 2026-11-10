//! The `syscall` gate (exception code 11, `SYS`).
//!
//! # How `syscall` works on LoongArch64
//!
//! The `syscall` instruction is unconfigured — there is nothing to enable and
//! no MSR/CSR to program. On execution the CPU (LoongArch Reference Manual
//! Vol. 1, `SYS`):
//!
//! 1. Saves the interrupted privilege level and interrupt enable in
//!    `PRMD.PPLV`/`PRMD.PIE`,
//! 2. Sets `CRMD.PLV = 0` and `CRMD.IE = 0` — the kernel runs the handler
//!    with interrupts off,
//! 3. Sets `ESTAT.EC = 11` and `ERA = PC of the syscall`,
//! 4. Enters at `EENTRY + 11 × VECSIZE`.
//!
//! # The register convention
//!
//! VelaOS follows the LoongArch *call* convention rather than the Linux
//! kernel's, so the numbers are the same on both sides of the gate:
//!
//! | Register | Role |
//! |---|---|
//! | `$r11` (`a7`) | syscall number |
//! | `$r4`–`$r9` (`a0`–`a5`) | arguments, in order |
//! | `$r4` (`a0`) on return | the result |
//! | `$r5` (`a1`) on return | the error flag (1 = the result is an errno) |
//!
//! The two-return-value shape (result in `a0`, error flag in `a1`) is the
//! VelaOS convention across architectures; note that Linux/LoongArch instead
//! returns `-errno` in `a0` and has no second register — the kernel's libc
//! layer owns that translation, not this crate.
//!
//! # Mechanism vs policy
//!
//! The trap stub in [`crate::idt`] saves every register into a
//! [`TrapFrame`](crate::idt::TrapFrame) and dispatches `EC = 11` to
//! [`dispatch`], which calls the kernel-registered handler. The handler
//! writes the result into `frame.a0` and the error flag into `frame.a1`;
//! the stub then restores all registers from the frame and returns to the
//! user with a single `ertn` (which restores `PLV`/`IE` from `PRMD` and jumps
//! to `ERA` — the instruction *after* the `syscall`).

use core::sync::atomic::{AtomicUsize, Ordering};

use crate::idt::TrapFrame;

/// The syscall exception code (`ESTAT.EC`), for readability at the dispatch
/// site. See [`crate::idt::ec::SYS`].
pub const EC: u8 = crate::idt::ec::SYS;

/// The address of the kernel's syscall dispatch function.
///
/// Set by [`install`] during boot. The trap handler calls through it for
/// `EC = 11`, so the kernel owns register reading and dispatch while this
/// module owns the entry mechanism.
static SYSCALL_DISPATCH: AtomicUsize = AtomicUsize::new(0);

/// Install the kernel's syscall dispatch function.
///
/// This must be called once, before any user code runs.
///
/// # Safety
///
/// `dispatch` is the kernel's syscall entry function: it reads the syscall
/// number from `frame.a7`, the arguments from `frame.a0..a5`, dispatches to
/// the handler, and writes the result into `frame.a0` and the error flag
/// into `frame.a1`.
pub unsafe fn install(dispatch: unsafe extern "C" fn(&mut TrapFrame)) {
    SYSCALL_DISPATCH.store(dispatch as usize, Ordering::Release);
}

/// Dispatch a syscall: called by the trap handler for `EC = 11`.
///
/// # Safety
///
/// `frame` must be the trap frame the assembly stub built for this syscall,
/// with the user's `a0`–`a7` in place and enough kernel stack mapped below
/// it for the handler to run.
pub unsafe fn dispatch(frame: &mut TrapFrame) {
    let target = SYSCALL_DISPATCH.load(Ordering::Acquire);
    if target == 0 {
        // No handler installed: report the gate as closed rather than jump
        // to zero.
        crate::serial::puts("\n*** syscall before gate::install ***\n");
        // SAFETY: the machine is being stopped on purpose: there is no
        // handler to call, and nothing after this runs.
        unsafe { crate::qemu_exit::power_off(1) };
    }
    // SAFETY: the pointer came from the caller, who got it from the stub.
    let target =
        unsafe { core::mem::transmute::<usize, unsafe extern "C" fn(&mut TrapFrame)>(target) };
    // SAFETY: the dispatch function's own contract (see `install`).
    unsafe { target(frame) };
}

/// Read the syscall number out of a frame (`a7`, `$r11`).
pub fn number(frame: &TrapFrame) -> u64 {
    frame.a7
}

/// Read the i-th argument (`a0`–`a5`, `$r4`–`$r9`); panics past the fifth.
pub fn argument(frame: &TrapFrame, index: usize) -> u64 {
    match index {
        0 => frame.a0,
        1 => frame.a1,
        2 => frame.a2,
        3 => frame.a3,
        4 => frame.a4,
        5 => frame.a5,
        _ => panic!("LoongArch syscalls take six arguments (a0–a5)"),
    }
}

/// Write the syscall result (`a0`) and error flag (`a1`).
pub fn respond(frame: &mut TrapFrame, result: u64, is_error: bool) {
    frame.a0 = result;
    frame.a1 = u64::from(is_error);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::idt::TrapFrame;

    #[test]
    fn number_and_arguments_read_the_right_slots() {
        let mut frame = TrapFrame::zeroed();
        frame.a7 = 42;
        frame.a0 = 1;
        frame.a1 = 2;
        frame.a2 = 3;
        frame.a3 = 4;
        frame.a4 = 5;
        frame.a5 = 6;

        assert_eq!(number(&frame), 42);
        for (index, expected) in [1u64, 2, 3, 4, 5, 6].iter().enumerate() {
            assert_eq!(argument(&frame, index), *expected);
        }
    }

    #[test]
    fn respond_writes_result_and_error_flag() {
        let mut frame = TrapFrame::zeroed();
        respond(&mut frame, 0x1234, false);
        assert_eq!(frame.a0, 0x1234);
        assert_eq!(frame.a1, 0);
        respond(&mut frame, 5, true);
        assert_eq!(frame.a0, 5);
        assert_eq!(frame.a1, 1);
    }

    #[test]
    fn argument_out_of_range_panics() {
        let frame = TrapFrame::zeroed();
        let result = std::panic::catch_unwind(|| argument(&frame, 6));
        assert!(result.is_err());
    }
}
