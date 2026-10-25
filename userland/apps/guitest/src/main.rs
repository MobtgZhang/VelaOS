//! The `guitest` guest binary.
//!
//! # What this file is, and is not
//!
//! The acceptance program itself lives in `lib.rs`, as a function that
//! *returns* what it observed. That split is what lets the hosted acceptance
//! test drive the real program and assert on the real result, instead of
//! reimplementing it in the test and testing the reimplementation.
//!
//! This file is only the guest entry point: it runs that function, prints the
//! outcome to the serial log, and powers the machine off. Nothing here decides
//! anything, which is deliberate — if this file had a decision in it, the test
//! and the guest would be running different programs.
//!
//! # Why it powers off
//!
//! A boot smoke test watches the serial log for a marker and has a hard
//! timeout. Reaching the marker and powering off turns "the run finished" into
//! a fact; a program that returns into a kernel with no more work to do leaves
//! the test to time out, and a timeout cannot say whether the program finished
//! and hung afterwards or never got that far.

#![cfg_attr(target_os = "none", no_std)]
// On bare metal the entry point is `_start`, and the binary must supply its own
// allocator and panic handler. Both are gated on the target rather than on
// `test`, because `cargo build --workspace` on the host builds this binary too
// and there it must remain an ordinary program.
#![cfg_attr(target_os = "none", no_main)]

#[cfg(target_os = "none")]
use vela_sys::runtime;

/// The guest heap.
///
/// The acceptance program builds strings for its log lines and owns a message
/// port, so it allocates. The allocator's policy lives in the SDK so every
/// VelaOS program grows its heap the same way.
#[cfg(target_os = "none")]
#[global_allocator]
static HEAP: runtime::GuestAllocator = runtime::GuestAllocator;

/// Report a panic and stop the machine.
///
/// Policy, not program logic: see `vela_sys::runtime`.
#[cfg(target_os = "none")]
#[panic_handler]
fn panic(info: &core::panic::PanicInfo<'_>) -> ! {
    runtime::panic(info)
}

#[cfg(target_os = "none")]
use core::fmt::Write as _;

#[cfg(target_os = "none")]
use vela_guitest::run;
#[cfg(target_os = "none")]
use vela_sys::api;

/// The bare-metal entry point.
#[cfg(target_os = "none")]
#[no_mangle]
pub extern "C" fn _start() -> ! {
    let mut log = Log;
    match run() {
        Ok(outcome) => {
            let _ = writeln!(
                log,
                "GUITEST-OK events={} gadget={} down={} up={} moves={} \
                 requests={} replies={} close={}",
                outcome.events,
                outcome.gadget_events,
                outcome.button_downs,
                outcome.button_ups,
                outcome.mouse_moves,
                outcome.requests,
                outcome.replies,
                outcome.close_events
            );
        }
        Err(f) => {
            // The step *and* the code: a bare error code says what went wrong
            // only to someone who already knows which call it came from.
            let _ = writeln!(log, "GUITEST-FAIL step={} code={}", f.step, f.code);
        }
    }
    vela_sys::power_off()
}

/// Writes to the kernel's serial log.
///
/// A user program has no console of its own: it has `putchar`, and on VelaOS
/// that lands on the serial port, which is where a boot log is read from.
#[cfg(target_os = "none")]
struct Log;

#[cfg(target_os = "none")]
impl core::fmt::Write for Log {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        // The SDK already has a `print`, which is the byte-at-a-byte spelling
        // of exactly this. Going through it rather than re-implementing it
        // means a program's console output uses the same path whether it is
        // formatting or not.
        api::print(s);
        Ok(())
    }
}

/// The hosted entry point.
///
/// Under `cargo test` there is no bare-metal entry and no console, so this
/// exists only to satisfy the binary's shape. The real work is in
/// [`vela_guitest::run`], which the acceptance test drives directly — running
/// it from `main` would mean the test exercised a different path than the one
/// it claims to.
#[cfg(not(target_os = "none"))]
fn main() {}
