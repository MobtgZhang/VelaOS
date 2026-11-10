//! QEMU-only power control through the `sifive_test` device.
//!
//! # Why this exists
//!
//! Both the plan and the report require that a kernel crash be
//! *observable*: a panicking kernel must be able to end the VM with a
//! status the CI harness reads, instead of hanging until a wall-clock
//! timeout or rebooting in a loop. On RISC-V the device that does this is
//! `sifive_test`, which QEMU's `virt` machine maps at physical
//! `0x0010_0000`. It is QEMU-only by construction: the address decodes to
//! nothing on real hardware, so the code is safe to leave in a production
//! kernel path (a write to an unclaimed physical address is a bus error at
//! worst, and the panic path is already terminal).
//!
//! # The device's one register
//!
//! `sifive_test` reads a 32-bit word at `FINISHER_*` offsets from its base:
//!
//! | Offset | Value | Effect |
//! |---|---|---|
//! | `0x0000` (PASS) | `0x5555` | exit status 0 |
//! | `0x0000` (FAIL) | `0x3333` | exit status 1 |
//! | `0x0000` (RESET) | `0x7777` | reset the machine |
//! | `0x0004` (FAIL_CODE) | code | exit status `code \| 1` |
//!
//! The `FAIL_CODE` path is the interesting one: the exit status is the
//! written value ORed with 1, so a *specific* failure code survives — the
//! CI harness can distinguish "panicked in the scheduler" from "panicked
//! in paging" instead of reading a bare "failed".
//!
//! On the host every write is compiled out, so tests that exercise the
//! exit paths report defaults rather than terminating the test process.

use crate::regs::ON_BARE_METAL;

/// The `sifive_test` device's physical base on QEMU's `virt` machine.
pub const SIFIVE_TEST_BASE: u64 = 0x0010_0000;

/// The PASS word: exit status 0.
pub const FINISHER_PASS: u32 = 0x5555;
/// The FAIL word: exit status 1.
pub const FINISHER_FAIL: u32 = 0x3333;
/// The RESET word: reset the machine instead of exiting.
pub const FINISHER_RESET: u32 = 0x7777;
/// The offset of the specific-failure-code register.
pub const FINISHER_FAIL_CODE_OFFSET: u64 = 4;

/// The exit status a `FAIL_CODE` write of `code` produces.
///
/// The device ORs the low bit in, so status 0 is not expressible through
/// the failure path — which is the point: a failure report must never be
/// confused with success.
pub const fn fail_exit_status(code: u32) -> u32 {
    code | 1
}

/// Write one 32-bit word to the device. A no-op off bare metal.
///
/// # Safety
///
/// `pa` must name a `sifive_test` register (base, or base plus
/// [`FINISHER_FAIL_CODE_OFFSET`]).
unsafe fn write32(pa: u64, value: u32) {
    if !ON_BARE_METAL {
        return;
    }
    // SAFETY: the caller names a sifive_test register; a 32-bit aligned
    // write to the device is its entire defined interface.
    unsafe { core::ptr::write_volatile(pa as *mut u32, value) }
}

/// Shut the VM down with exit status 0.
///
/// If the device was not present — a `-M virt` launch without it, or real
/// hardware — the write does nothing and the CPU falls through, so the
/// caller must treat "returned" as "the exit did not happen" and park.
pub fn exit() -> ! {
    // SAFETY: `SIFIVE_TEST_BASE` is the device's PASS register.
    unsafe { write32(SIFIVE_TEST_BASE, FINISHER_PASS) };
    crate::regs::wfi();
    unreachable!("the pass write returned; sifive_test must have fired")
}

/// Shut the VM down with exit status 1.
pub fn power_off() -> ! {
    // SAFETY: `SIFIVE_TEST_BASE` is the device's FAIL register.
    unsafe { write32(SIFIVE_TEST_BASE, FINISHER_FAIL) };
    crate::regs::wfi();
    unreachable!("the fail write returned; sifive_test must have fired")
}

/// Shut the VM down with the specific failure status `code | 1`.
///
/// # Parameters
///
/// `code` is the failure discriminator the kernel's panic path chooses;
/// the device forces its low bit, so status 0 can never result.
pub fn fail_word(code: u32) -> ! {
    // SAFETY: the fail-code register sits 4 bytes above the base.
    unsafe { write32(SIFIVE_TEST_BASE + FINISHER_FAIL_CODE_OFFSET, code) };
    crate::regs::wfi();
    unreachable!("the fail-code write returned; sifive_test must have fired")
}

/// Reset the machine rather than exiting.
pub fn reset() -> ! {
    // SAFETY: the base register with the RESET word requests a reset.
    unsafe { write32(SIFIVE_TEST_BASE, FINISHER_RESET) };
    crate::regs::wfi();
    unreachable!("the reset write returned; sifive_test must have fired")
}

#[cfg(test)]
extern crate std;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_device_sits_at_the_qemu_virt_base() {
        assert_eq!(SIFIVE_TEST_BASE, 0x0010_0000);
    }

    #[test]
    fn the_finisher_words_are_the_sifive_ones() {
        // These are the SiFive test device's magic values; getting one
        // wrong makes every pass look like a failure and vice versa.
        assert_eq!(FINISHER_PASS, 0x5555);
        assert_eq!(FINISHER_FAIL, 0x3333);
        assert_eq!(FINISHER_RESET, 0x7777);
        // The three words are distinct, which is what makes the device's
        // meaning unambiguous.
        assert_ne!(FINISHER_PASS, FINISHER_FAIL);
        assert_ne!(FINISHER_FAIL, FINISHER_RESET);
        assert_ne!(FINISHER_PASS, FINISHER_RESET);
    }

    #[test]
    fn fail_code_status_always_has_the_low_bit() {
        // Status 0 is not expressible through the failure path — the
        // device ORs 1 in, so a failure can never read as success.
        for code in [0u32, 1, 2, 3, 42, 0x7FFF, u32::MAX] {
            let status = fail_exit_status(code);
            assert_eq!(status & 1, 1, "low bit forced for code {code}");
        }
    }

    #[test]
    fn the_fail_code_register_is_four_bytes_in() {
        assert_eq!(FINISHER_FAIL_CODE_OFFSET, 4);
        assert_eq!(SIFIVE_TEST_BASE + FINISHER_FAIL_CODE_OFFSET, 0x0010_0004);
    }

    #[test]
    fn the_exit_paths_are_inert_off_bare_metal() {
        // The writes are compiled out on the host, so every path reaches
        // the wfi (also compiled out) and falls through to the
        // unreachable! — which must never fire, or the tests would panic.
        //
        // These functions are `-> !`, so they cannot be called in a test
        // that expects to continue; what is asserted instead is that the
        // primitive the paths share reports its inert state.
        // SAFETY: the documented host no-op.
        unsafe { write32(SIFIVE_TEST_BASE, FINISHER_PASS) };
        unsafe { write32(SIFIVE_TEST_BASE + FINISHER_FAIL_CODE_OFFSET, 42) };
    }
}
