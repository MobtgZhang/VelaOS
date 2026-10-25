//! `init` — the first user-space program.
//!
//! # What `init` is for
//!
//! On AmigaOS, `init` is the first process a boot produces: it runs before
//! `workbench`, before the shell, before anything a user launched. It brings
//! the machine's *namespaces* into existence — the assigns, the `SYS:` tree,
//! the current directory — and then gets out of the way.
//!
//! VelaOS's kernel already mounts its filesystems before any user program
//! runs, because a kernel that let a user program choose what to mount would
//! be a kernel with a sandbox escape in it. So `init` does not mount. What it
//! does is the part that genuinely is user-space policy:
//!
//! * it **populates** the DOS namespace — creating the `SYS:` and `RAM:`
//!   directories a program expects to find, so that a program opening
//!   `SYS:System/Prefs/Startup-Prefs` gets a sensible "no such file" rather
//!   than a resolver error;
//! * it **seeds** `RAM:` with the tree a fresh desktop wants;
//! * and then it **starts the desktop**, which is the thing that matters.
//!
//! The work itself is in [`vela_init::run`], in the library half of this
//! crate, so that the hosted tests can assert on what it *did* rather than on
//! what it logged. This file is only the part that can only happen on the
//! machine: the entry point, the handover, and the power-off.
//!
//! # Why the desktop is linked rather than spawned
//!
//! There is no `RunSeg` in v0.1.0, and adding one for a single user would be a
//! mechanism with exactly one user and no second user to justify it. So the
//! desktop is linked in and called, through one named function. The seam is
//! deliberate: [`hand_over_to_desktop`] is the single line that becomes a
//! spawn when there is a second thing to spawn, and everything above it already
//! returns — so the change is local rather than a redesign.
//!
//! # Why it exits rather than loops
//!
//! A daemon that never terminates is a daemon that can never be restarted. If
//! the desktop wedges, the supervisor is `init` — and a supervisor that is
//! itself wedged cannot notice. So `init` does its work, reports it, hands
//! over, and powers the machine off when the session ends.
//!
//! # Reporting
//!
//! Every step is reported on the console. Not for the user: for whoever reads
//! a boot log. A guest that fails silently at this stage produces a black
//! screen, and a black screen is the single most expensive thing to diagnose.

#![cfg_attr(target_os = "none", no_std)]
// On bare metal the entry point is `_start`, not `main`. Gated on the target
// rather than unconditional because `cargo test` builds this binary for the
// host with a test harness, which needs its own `main` — a crate-level
// `no_main` would remove it and the link would fail.
#![cfg_attr(target_os = "none", no_main)]

#[cfg(target_os = "none")]
use vela_sys::runtime;

/// The guest heap.
///
/// A bare-metal `no_std` binary needs one to use `String`, `Vec` or
/// `format!` at all, and this program uses all three. The allocator and the
/// panic handler are policy rather than program logic, so the SDK provides
/// them and the program opts in — see `vela_sys::runtime`.
#[cfg(target_os = "none")]
#[global_allocator]
static HEAP: runtime::GuestAllocator = runtime::GuestAllocator;

/// Report a panic and stop the machine.
///
/// The policy lives in the SDK so every VelaOS program fails the same way:
/// the message on the console, then a distinct exit status so the boot smoke
/// test can tell a crash from a clean finish.
#[cfg(target_os = "none")]
#[panic_handler]
fn panic(info: &core::panic::PanicInfo<'_>) -> ! {
    runtime::panic(info)
}

/// The session: bootstrap, then hand over to the desktop.
///
/// One function so that "what `init` does" has a single answer.
#[cfg(target_os = "none")]
pub fn session() -> ! {
    use vela_init::run;
    use vela_sys::api;

    log("init: starting");

    let report = run();
    log("init: RAM:");
    api::print_int(report.ram_ready as u64);
    log(" SYS:");
    api::print_int(report.sys_ready as u64);
    log(" failed:");
    api::print_int(report.failures as u64);
    log("\n");

    if !report.is_usable() {
        // Starting a desktop with no `SYS:` would produce windows that cannot
        // find their configuration, which looks like a graphics bug. Saying so
        // and stopping is more useful than a screen full of misbehaviour.
        log("init: no usable SYS: — halting\n");
        api::power_off();
    }

    log("init: starting workbench\n");
    hand_over_to_desktop();

    // The desktop returned. In v0.1.0 that means it decided the session was
    // over — most likely because the user closed the last window — so the
    // machine stops rather than sitting on a desktop nobody is driving.
    log("init: session ended\n");
    api::power_off()
}

/// Where the desktop takes over.
///
/// A separate function so the *handover* is one identifiable step. When the
/// desktop becomes a real second task, this body becomes the spawn and
/// everything around it stays exactly where it is.
#[cfg(target_os = "none")]
fn hand_over_to_desktop() {
    vela_workbench::run();
}

/// The bare-metal entry point.
#[cfg(target_os = "none")]
#[no_mangle]
pub extern "C" fn _start() -> ! {
    session()
}

/// Writes `s` to the console.
///
/// `init`'s only output mechanism, and deliberately the whole of it: a boot
/// log that a program could colour, wrap or paginate would be a boot log
/// nobody could grep.
#[cfg(target_os = "none")]
fn log(s: &str) {
    vela_sys::api::print(s);
}

/// The hosted entry point.
///
/// A placeholder for the same reason `guitest`'s is: the real entry point is
/// [`session`], and the hosted tests drive `run` so they exercise the work
/// rather than the handover.
#[cfg(not(target_os = "none"))]
fn main() {}
