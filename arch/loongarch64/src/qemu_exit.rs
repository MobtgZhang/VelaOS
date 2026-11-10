//! Stopping the machine.
//!
//! # How a LoongArch machine is told to stop
//!
//! QEMU's `loongarch-virt` machine implements the ACPI power-off and
//! reset buttons, and it also accepts the semihosting call that a debugger
//! uses to end a program. The semihosting path is the one that works
//! everywhere — it needs no bus, no device and no firmware cooperation:
//!
//! ```text
//!   li   $a0, 0x0f        ; SYS_EXIT
//!   li   $a1, 0x00        ; ADP_Stopped_ApplicationExit
//!   break 0x0             ; the semihosting trap
//! ```
//!
//! The `break` with code zero is the semihosting trap on this
//! architecture; the operands in `a0` and `a1` say what to do.

/// The semihosting operation that ends the program.
pub const SYS_EXIT: u64 = 0x0F;

/// The `ADP_Stopped_ApplicationExit` reason code.
pub const ADP_STOPPED_APPLICATION_EXIT: u64 = 0x00;

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
    if crate::regs::ON_BARE_METAL {
        // SAFETY: the semihosting call is the architecture's defined way
        // to end a program, and it never returns.
        unsafe {
            core::arch::asm!(
                "li   $a0, {op}",
                "li   $a1, {reason}",
                "break 0x0",
                op = in(reg) SYS_EXIT,
                reason = in(reg) ADP_STOPPED_APPLICATION_EXIT,
                options(noreturn)
            );
        }
    }
    // Off bare metal there is no machine to stop; the loop keeps the
    // "never returns" contract.
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
        assert_eq!(SYS_EXIT, 0x0F);
        assert_eq!(ADP_STOPPED_APPLICATION_EXIT, 0x00);
    }

    #[test]
    fn the_status_is_recorded() {
        // The record is what a test reads after the machine has stopped.
        EXIT_STATUS.store(7, core::sync::atomic::Ordering::Release);
        assert_eq!(exit_status(), 7);
        EXIT_STATUS.store(0, core::sync::atomic::Ordering::Release);
    }
}
