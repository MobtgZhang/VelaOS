//! Stopping the machine.
//!
//! # How an AArch64 machine is told to stop
//!
//! QEMU's `virt` machine implements the semihosting call that a debugger
//! uses to end a program:
//!
//! ```text
//!   mov   x0, #0x18        ; SYS_EXIT
//!   mov   x1, #0x26        ; ADP_Stopped_ApplicationExit
//!   hlt   #0xF000          ; the semihosting trap
//! ```
//!
//! The status travels in `x2` for the A64 variant, so the machine can tell
//! a program that finished from one that was killed — which is what a boot
//! smoke test needs.

/// The semihosting operation that ends the program.
pub const SYS_EXIT: u64 = 0x18;

/// The `ADP_Stopped_ApplicationExit` reason code.
pub const ADP_STOPPED_APPLICATION_EXIT: u64 = 0x26;

/// The exit status the machine reports.
static EXIT_STATUS: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

/// The status the machine was asked to stop with.
pub fn exit_status() -> u32 {
    EXIT_STATUS.load(core::sync::atomic::Ordering::Acquire)
}

/// Stop the machine with a status.
///
/// The status travels through the semihosting call, so a test can tell a
/// program that finished from one that was killed.
///
/// # Safety
///
/// The machine stops: nothing after this call runs.
pub unsafe fn power_off(status: u32) -> ! {
    EXIT_STATUS.store(status, core::sync::atomic::Ordering::Release);
    // The assembly is compiled only for the real target: an AArch64
    // instruction has no meaning on a host assembler.
    #[cfg(target_arch = "aarch64")]
    // SAFETY: the semihosting call is the architecture's defined way to
    // end a program, and it never returns.
    unsafe {
        // The operands are named 64-bit registers explicitly: a
        // general-register operand could be allocated as a 32-bit
        // register, and `mov` needs both sides the same width.
        core::arch::asm!(
            "mov  x0, x3",
            "mov  x1, x4",
            "mov  x2, x5",
            "hlt  #0xF000",
            in("x3") SYS_EXIT,
            in("x4") ADP_STOPPED_APPLICATION_EXIT,
            in("x5") status,
            options(noreturn)
        );
    }
    #[cfg(not(target_arch = "aarch64"))]
    let _ = status;
    // Off bare metal there is no machine to stop; the loop keeps the
    // "never returns" contract.
    #[cfg(not(target_arch = "aarch64"))]
    loop {
        core::hint::spin_loop();
    }
}

#[cfg(test)]
extern crate std;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_semihosting_operands_are_the_ones_the_spec_names() {
        assert_eq!(SYS_EXIT, 0x18);
        assert_eq!(ADP_STOPPED_APPLICATION_EXIT, 0x26);
    }

    #[test]
    fn the_status_is_recorded() {
        // The record is what a test reads after the machine has stopped.
        EXIT_STATUS.store(7, core::sync::atomic::Ordering::Release);
        assert_eq!(exit_status(), 7);
        EXIT_STATUS.store(0, core::sync::atomic::Ordering::Release);
    }
}
