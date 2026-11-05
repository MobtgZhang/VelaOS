//! The `shell` binary — VelaOS's command-line interpreter.
//!
//! A shell around [`vela_shell::Shell`], so that the shell's logic lives in a
//! library and this file contains nothing but the entry point. The bare-metal
//! `_start` is here rather than in the library for the same reason: an entry
//! point is a property of *this* binary, and putting it in a library would
//! mean every program that linked the shell also inherited its entry point.

// On bare metal a guest brings its own panic handler; on the host it uses the
// standard one, because the standard one exists.
#![cfg_attr(target_os = "none", no_std)]
// On bare metal the entry point is `_start`, not `main`, and this binary
// supplies its own panic handler and allocator. Both are cfg-gated on the
// target rather than on `test` because `cargo build --workspace` on the host
// builds this binary too, and there it must remain an ordinary program.
#![cfg_attr(target_os = "none", no_main)]

#[cfg(target_os = "none")]
use vela_sys::runtime;

/// The guest heap.
///
/// Required to use `Vec`, `String` or `format!` at all, and the shell uses
/// all three. The allocator's policy lives in the SDK so every VelaOS program
/// grows its heap the same way.
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

/// The bare-metal entry point.
///
/// Runs the shell, then powers the machine off. A shell that stopped the
/// machine itself would mean the shell's shutdown path had to be right for
/// the machine to shut down, and a guest that could not shut down cleanly
/// would look like a hang.
#[cfg(target_os = "none")]
#[no_mangle]
pub extern "C" fn _start() -> ! {
    vela_shell::Shell::new().run()
}

/// The hosted entry point.
///
/// A placeholder: under `cargo test` there is no bare metal and no session,
/// and the tests drive the shell's own functions so that they exercise the
/// logic rather than the shell.
#[cfg(not(target_os = "none"))]
fn main() {}
