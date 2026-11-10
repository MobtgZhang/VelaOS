//! Stopping the machine.
//!
//! # How an AArch64 machine is told to stop
//!
//! QEMU's `virt` machine implements the semihosting call that a debugger
//! uses to end a program:
//!
//! ```text
//!   x0 = 0x18              ; SYS_EXIT
//!   x1 = &block            ; word 0: ADP_Stopped_ApplicationExit (0x20026)
//!                          ; word 1: the exit status
//!   hlt #0xF000            ; the semihosting trap
//! ```
//!
//! The A64 variant passes a *pointer to a two-word block*, not the reason
//! and the status in registers: `x1` points at the block, whose first word
//! is the reason code and whose second is the status. (The A32 variant is
//! the other way round — reason in `x1`, status in `x2` — which is exactly
//! the kind of difference that a half-remembered convention gets wrong.)
//!
//! The status travels in the block, so the machine can tell a program that
//! finished from one that was killed — which is what a boot smoke test
//! needs.

/// The semihosting operation that ends the program.
pub const SYS_EXIT: u64 = 0x18;

/// The `ADP_Stopped_ApplicationExit` reason code.
pub const ADP_STOPPED_APPLICATION_EXIT: u64 = 0x20026;

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
        // The parameter block the specification asks `x1` to point at: the
        // reason code first, the exit status second. It lives on the
        // kernel's own stack, which is mapped and writable, and the
        // semihosting call reads it before the machine stops.
        let block = [ADP_STOPPED_APPLICATION_EXIT, status as u64];
        core::arch::asm!(
            "mov  x0, x3",
            "mov  x1, x4",
            "hlt  #0xF000",
            in("x3") SYS_EXIT,
            in("x4") block.as_ptr() as u64,
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
        // The A64 reason code is 0x20026 — the A32 value (0x26) is a
        // different encoding of the same reason.
        assert_eq!(ADP_STOPPED_APPLICATION_EXIT, 0x20026);
    }

    #[test]
    fn the_status_is_recorded() {
        // The record is what a test reads after the machine has stopped.
        EXIT_STATUS.store(7, core::sync::atomic::Ordering::Release);
        assert_eq!(exit_status(), 7);
        EXIT_STATUS.store(0, core::sync::atomic::Ordering::Release);
    }
}
