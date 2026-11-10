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
//! # The device's single register
//!
//! The whole protocol is one 32-bit write at offset 0 of the base
//! (QEMU's `hw/misc/sifive_test.c`). The low half selects the action and
//! the high half carries the exit status:
//!
//! ```text
//! 31            16 15          0
//! +---------------+-------------+
//! |  exit status  |   action    |
//! +---------------+-------------+
//!                   0x5555 = PASS  -> exit with the status above
//!                   0x3333 = FAIL  -> exit with the status above
//!                   0x7777 = RESET -> reset the machine instead
//! ```
//!
//! The exit status is exactly the word's high half — QEMU's
//! `qemu_system_shutdown_request_with_code` receives `(value >> 16) &
//! 0xFFFF` and uses it as the process exit status. A write whose action
//! field is none of the three words, or whose address is not offset 0, is
//! logged as a guest error and ignored — which is what makes "the device
//! was not there" a fall-through rather than an exit.
//!
//! On the host every write is compiled out, so nothing here can terminate
//! the test process; the tests pin the *encoding*, which is the only
//! part that can be exercised without a VM.

use crate::regs::{self, ON_BARE_METAL};

/// The `sifive_test` device's physical base on QEMU's `virt` machine.
pub const SIFIVE_TEST_BASE: u64 = 0x0010_0000;

/// The PASS action: shut the VM down, reporting the status from the high
/// half (0 for a plain `exit_success`).
pub const FINISHER_PASS: u32 = 0x5555;
/// The FAIL action: shut the VM down as a guest panic, reporting the
/// status from the high half.
pub const FINISHER_FAIL: u32 = 0x3333;
/// The RESET action: reset the machine rather than exiting.
pub const FINISHER_RESET: u32 = 0x7777;

/// The bit position of the exit-status field in a finisher word.
pub const EXIT_CODE_SHIFT: u32 = 16;

/// Compose the word the device expects: `kind` in the low half, `code` in
/// the high half.
///
/// `code` is truncated to 16 bits — all the device can express. The three
/// action words are themselves 16-bit values, so they never collide with
/// the status field.
pub const fn encode_finisher(kind: u32, code: u32) -> u32 {
    ((code & 0xFFFF) << EXIT_CODE_SHIFT) | kind
}

/// The word that exits the VM with status `code`, through the panic
/// (FAIL) action.
pub const fn fail_word(code: u32) -> u32 {
    encode_finisher(FINISHER_FAIL, code)
}

/// The word that exits the VM with status `code`, through the clean
/// (PASS) action.
pub const fn pass_word(code: u32) -> u32 {
    encode_finisher(FINISHER_PASS, code)
}

/// The word that resets the machine.
pub const fn reset_word() -> u32 {
    FINISHER_RESET
}

/// Write one 32-bit word to the device. A no-op off bare metal.
///
/// # Safety
///
/// `pa` must be [`SIFIVE_TEST_BASE`] — the device's only register.
unsafe fn write32(pa: u64, value: u32) {
    if !ON_BARE_METAL {
        return;
    }
    // SAFETY: the caller names the sifive_test register; a 32-bit aligned
    // write to the device is its entire defined interface.
    unsafe { core::ptr::write_volatile(pa as *mut u32, value) }
}

/// Park the hart forever. The safe terminal state when the exit did not
/// happen: the VM stays up and the CI watchdog kills it, which is a
/// *louder* failure than a bogus exit status.
fn park() -> ! {
    loop {
        // `wfi` is a no-op off bare metal, so on the host this spins —
        // which is why the exit functions are never called from a test.
        regs::wfi();
    }
}

/// Shut the VM down with the *failure* status `code`.
///
/// The kernel's panic path calls this, mirroring the x86-64 crate's
/// `power_off`: a panic must be visible to CI as a non-zero exit status,
/// with `code` distinguishing *where* the kernel died. `code` is truncated
/// to the 16 bits the device can carry; the low bit is deliberately not
/// forced, because the device needs no x86-style `| 1` encoding — the
/// status arrives exactly as written.
///
/// If the device is absent the write does nothing and the hart parks in a
/// `wfi` loop; the harness treats a timeout that way as a failure, which
/// keeps "panicked" distinguishable from "hung" only by the message
/// printed before the exit.
pub fn power_off(code: u32) -> ! {
    // SAFETY: `SIFIVE_TEST_BASE` is the device's register.
    unsafe { write32(SIFIVE_TEST_BASE, fail_word(code)) };
    park()
}

/// Shut the VM down with exit status 0.
///
/// The success path of a boot smoke test: the kernel prints its banner,
/// verifies what it set out to verify, and asks the VM to exit cleanly.
pub fn exit_success() -> ! {
    // SAFETY: as in [`power_off`].
    unsafe { write32(SIFIVE_TEST_BASE, pass_word(0)) };
    park()
}

/// Reset the machine rather than exiting.
///
/// Used by a reboot path: QEMU re-runs the firmware and the kernel starts
/// over. The exit status field is written as zero — the device ignores it
/// for the RESET action.
pub fn reset() -> ! {
    // SAFETY: as in [`power_off`].
    unsafe { write32(SIFIVE_TEST_BASE, reset_word()) };
    park()
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
        // meaning unambiguous — and none of them collides with a status
        // in the high half of a composed word.
        assert_ne!(FINISHER_PASS, FINISHER_FAIL);
        assert_ne!(FINISHER_FAIL, FINISHER_RESET);
        assert_ne!(FINISHER_PASS, FINISHER_RESET);
    }

    #[test]
    fn the_status_lives_in_the_high_half_of_the_word() {
        // This is the shape QEMU's sifive_test decodes: status = value >>
        // 16, action = value & 0xFFFF. A panic code of 2 must be
        // recoverable from the written word, action and all.
        let word = fail_word(2);
        assert_eq!(word & 0xFFFF, FINISHER_FAIL);
        assert_eq!(word >> EXIT_CODE_SHIFT, 2);
        // A clean exit is the PASS action with status 0.
        let word = pass_word(0);
        assert_eq!(word, FINISHER_PASS);
        assert_eq!(word >> EXIT_CODE_SHIFT, 0);
    }

    #[test]
    fn the_status_field_is_sixteen_bits() {
        // The device can express only 16 bits of status; a code wider
        // than that must be truncated by the encoder, not aliased into
        // the action field.
        for code in [0xFFFFu32, 0x1_0000, u32::MAX] {
            let word = fail_word(code);
            assert_eq!(
                word & FINISHER_FAIL,
                FINISHER_FAIL,
                "the action field is never touched by the status"
            );
            assert_eq!(word >> EXIT_CODE_SHIFT, code & 0xFFFF);
        }
    }

    #[test]
    fn the_exit_statuses_are_recoverable() {
        // The harness reads the process exit status, so the codes the
        // kernel picks must survive the encoding exactly.
        assert_eq!(fail_word(0) >> EXIT_CODE_SHIFT, 0);
        assert_eq!(fail_word(1) >> EXIT_CODE_SHIFT, 1);
        assert_eq!(pass_word(0) >> EXIT_CODE_SHIFT, 0);
    }

    #[test]
    fn the_reset_word_stands_alone() {
        assert_eq!(reset_word(), FINISHER_RESET);
    }

    #[test]
    fn the_write_is_inert_off_bare_metal() {
        // The volatile surface must be callable on the host without
        // touching an MMIO window; only the encoding above is asserted,
        // and the call below is a no-op unless ON_BARE_METAL holds.
        // SAFETY: the documented host no-op.
        unsafe { write32(SIFIVE_TEST_BASE, fail_word(1)) };
    }
}
