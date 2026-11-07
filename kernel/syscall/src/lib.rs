//! The VelaOS syscall boundary.
//!
//! # What this crate is for
//!
//! It is the boundary between ring 3 and ring 0. A user program reaches the
//! kernel only through here, and everything the kernel does on a user's
//! behalf — reading a message, opening a library, drawing a rectangle — goes
//! through here too.
//!
//! # The three rules
//!
//! 1. **A user pointer is never dereferenced by the kernel.** It is copied
//!    *from* or *to* with the helpers in [`copy`], which validate the range
//!    and probe the access.
//! 2. **A user program never receives a kernel pointer.** It receives an
//!    opaque handle: a port id, a library handle, a window id. The handle
//!    is meaningless outside the kernel, which is exactly the property a
//!    pointer into another task's memory would lack.
//! 3. **A syscall never panics.** A panic at ring 0 on a user stack takes the
//!    kernel down. Every error path returns a [`SyscallResult`].
//!
//! # The gate
//!
//! The `syscall` instruction is entered through the MSRs described in
//! [`gate`]. The gate saves the user registers, calls the dispatcher, and
//! returns. It is the only place where the user and kernel register sets
//! meet.
//!
//! # Module map
//!
//! | Module | Responsibility |
//! |---|---|
//! | [`numbers`] | The syscall numbers, their groups, and the calling convention |
//! | [`copy`] | The user-copy helpers, with range validation and probing |
//! | [`dispatch`] | The dispatch table and the Exec-library handlers |
//! | [`dos_handlers`] | The `dos.library` handlers |
//! | [`graphics_handlers`] | The `graphics.library` handlers |
//! | [`layers_handlers`] | The `layers.library` handlers |
//! | [`intuition_handlers`] | The `intuition.library` handlers |
//! | [`utility_handlers`] | The `utility.library` handlers |
//! | [`gate`] | The `syscall` entry point and the MSR setup |
//! | [`ids`] | The opaque handles user space holds |

#![no_std]

extern crate alloc;

// Off bare metal the host build links the standard library, which the
// user-copy helpers use to answer "is this address mapped?" without
// faulting the test run.
#[cfg(not(target_os = "none"))]
extern crate std;

pub mod copy;
pub mod dispatch;
pub mod dos_handlers;
pub mod gate;
pub mod graphics_handlers;
pub mod icon_handlers;
pub mod ids;
pub mod intuition_handlers;
pub mod layers_handlers;
pub mod numbers;
pub mod task_space;
pub mod userspace;
pub mod utility_handlers;
pub mod workbench_handlers;

use alloc::boxed::Box;

#[cfg(target_os = "none")]
use spin::Mutex;

use crate::copy::ProbeState;

/// The probe state.
///
/// # Per-CPU, not per-process
///
/// The probe state belongs to a *CPU*, not to the kernel as a whole. On
/// bare metal there is one CPU, so a single global instance is correct — and
/// it must be a single instance, because the fault handler runs on the same
/// CPU with no way to be told which probe it is serving.
///
/// On the host there are many threads, and the host's threads are the
/// analogue of the kernel's CPUs: each one runs its own copy of the fault
/// handler's context. A single global instance would mean one thread's
/// probe blocks another's, and the second thread's copy would fail for no
/// reason. So the host build uses a thread-local instance.
#[cfg(target_os = "none")]
pub fn probe_state() -> &'static ProbeState {
    // The state is leaked once so that the reference is stable and the
    // lock-free fast path in the fault handler can read it.
    //
    // `Mutex::get_mut` would need `&mut self`, which the fault handler cannot
    // provide.
    static STATE: Mutex<Option<&'static ProbeState>> = Mutex::new(None);
    let mut slot = STATE.lock();
    if slot.is_none() {
        let leaked: &'static ProbeState = Box::leak(Box::new(ProbeState::new()));
        *slot = Some(leaked);
    }
    slot.unwrap()
}

/// The probe state for the calling thread.
///
/// See the bare-metal version for why this is thread-local.
#[cfg(not(target_os = "none"))]
pub fn probe_state() -> &'static ProbeState {
    std::thread_local! {
        // The `Cell` holds the leaked reference. The leak is deliberate: the
        // reference must live as long as the thread, and the fault handler
        // — which on the host is the host's own signal machinery — needs a
        // stable address.
        static STATE: std::cell::Cell<Option<&'static ProbeState>> =
            const { std::cell::Cell::new(None) };
    }
    STATE.with(|cell| {
        if cell.get().is_none() {
            let leaked: &'static ProbeState = Box::leak(Box::new(ProbeState::new()));
            cell.set(Some(leaked));
        }
        cell.get().unwrap()
    })
}

/// The result of a syscall, re-exported for convenience.
pub use numbers::SyscallResult;

/// The arguments of a syscall, re-exported for convenience.
pub use numbers::SyscallArgs;

/// The machine's hostname string, leaked for the kernel's lifetime.
///
/// Set once during boot alongside [`hostname`].
static HOSTNAME_STR: core::sync::atomic::AtomicU64 =
    core::sync::atomic::AtomicU64::new(0);

/// The console write function, set once during boot.
///
/// Points at `serial::puts` on bare metal, or a test sink on the host.
static CONSOLE_WRITE: core::sync::atomic::AtomicU64 =
    core::sync::atomic::AtomicU64::new(0);

/// The machine's hostname.
pub fn hostname() -> &'static str {
    let ptr = HOSTNAME_STR.load(core::sync::atomic::Ordering::Acquire);
    if ptr == 0 {
        return "vela";
    }
    // SAFETY: the pointer was stored by `set_hostname` during boot and
    // names a `&'static str` that lives for the kernel's lifetime.
    unsafe { *(ptr as *const &'static str) }
}

/// Set the machine's hostname.
///
/// # Safety
///
/// Must be called exactly once, during boot, before any task runs.
pub unsafe fn set_hostname(name: &'static str) {
    // SAFETY: written once during boot, before any task runs.
    let leaked: &'static &'static str = Box::leak(Box::new(name));
    HOSTNAME_STR.store(leaked as *const &'static str as u64, core::sync::atomic::Ordering::Release);
}

/// Set the console write function.
///
/// # Safety
///
/// Must be called exactly once, during boot, before any task runs.
pub unsafe fn set_console_write(f: fn(&str)) {
    // SAFETY: written once during boot, before any task runs.
    CONSOLE_WRITE.store(f as *const () as usize as u64, core::sync::atomic::Ordering::Release);
}

/// Print a string to the console.
///
/// No-op if no console write function has been installed yet.
pub fn print_console(s: &str) {
    let f = CONSOLE_WRITE.load(core::sync::atomic::Ordering::Acquire);
    if f != 0 {
        // SAFETY: the only value ever stored is a `fn(&str)` address,
        // installed during boot before interrupts are enabled.
        let f: fn(&str) = unsafe { core::mem::transmute(f) };
        f(s);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_probe_state_is_stable() {
        // The fault handler and the copy helpers must agree on the same
        // state object, so it must be a single, fixed instance.
        let a = probe_state() as *const ProbeState;
        let b = probe_state() as *const ProbeState;
        assert_eq!(a, b);
    }

    #[test]
    fn the_probe_state_starts_idle() {
        let state = probe_state();
        // A previous test may have left it set, but it must not be *in
        // progress* — that would make every subsequent copy fail.
        assert!(!state
            .in_progress
            .load(core::sync::atomic::Ordering::Acquire));
    }
}
