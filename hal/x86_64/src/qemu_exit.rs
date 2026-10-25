//! QEMU-only power control, used by the panic handler and the exception
//! handlers.
//!
//! # Why this exists
//!
//! Both the plan and the report require that a kernel crash be *observable*:
//! "panic handler that prints a backtrace-ish summary and can power off QEMU
//! (`isa-debug-exit`) for CI". Without a defined exit path, a panicking kernel
//! either hangs until a wall-clock timeout or reboots in a loop, and the CI
//! test cannot tell "panicked as expected" from "hung".
//!
//! # The port write
//!
//! QEMU's `isa-debug-exit` device takes a single byte on an I/O port; the value
//! encodes `(exit_code << 1) | 1`. Writing that byte terminates the VM with that
//! status, which `tools/xtask` and `tests/boot-smoke` read to decide pass/fail.
//! On real hardware the port write goes to an unclaimed I/O port and is a
//! harmless no-op, so the code is safe to leave in a production kernel path.

use crate::regs;

/// The I/O port QEMU maps the debug-exit device to.
pub const DEBUG_EXIT_PORT: u16 = 0xF4;

/// Encode an exit code the way `isa-debug-exit` expects.
///
/// The low bit is always set; the status is the code shifted left by one. So
/// code 0 becomes 1, code 1 becomes 3, and so on.
pub const fn encode_exit_code(code: u32) -> u8 {
    ((code << 1) | 1) as u8
}

/// Shut the VM down, reporting `code` as the exit status.
///
/// The `isa-debug-exit` device only exists when QEMU was launched with
/// `-device isa-debug-exit`; without it the write is discarded by the emulated
/// platform and the VM keeps running, so this is deliberately not a hard
/// guarantee. `tools/xtask` always adds the device.
pub fn power_off(code: u32) -> ! {
    // SAFETY: writing a single byte to a fixed port; on QEMU it triggers the
    // debug-exit device, elsewhere it is ignored.
    unsafe { regs::outb(DEBUG_EXIT_PORT, encode_exit_code(code)) };

    // If the device was not present, the write above did nothing and the CPU
    // must not continue into an inconsistent state. Halting is the safe
    // fallback; the watchdog in `xtask qemu` eventually kills the VM.
    crate::regs::hlt();
    unreachable!("hlt returned; the debug-exit device must have fired")
}

#[cfg(test)]
extern crate std;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_code_encoding_matches_qemu() {
        // The low bit is always 1 and the status sits above it. Getting this
        // wrong makes every pass look like a failure and vice versa.
        assert_eq!(encode_exit_code(0), 0b01);
        assert_eq!(encode_exit_code(1), 0b11);
        assert_eq!(encode_exit_code(2), 0b101);
        assert_eq!(encode_exit_code(3), 0b111);
    }

    #[test]
    fn exit_status_is_recoverable_from_the_written_byte() {
        for code in [0u32, 1, 2, 42, 127] {
            let byte = encode_exit_code(code);
            assert_eq!((byte >> 1) as u32, code);
            assert_eq!(byte & 1, 1, "low bit must always be set");
        }
    }

    #[test]
    fn debug_port_is_the_documented_one() {
        assert_eq!(DEBUG_EXIT_PORT, 0xF4);
    }
}
