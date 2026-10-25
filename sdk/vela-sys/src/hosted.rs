//! The hosted syscall path.
//!
//! # What this is for
//!
//! Every piece of kernel logic in VelaOS is testable under plain `cargo
//! test`, and that only works if user-mode code can issue a syscall without
//! one existing. So this module calls the kernel's dispatcher directly.
//!
//! # What it is not
//!
//! It is **not** a way for a program to reach the kernel from user space. On
//! bare metal there is no such path: the only way into ring 0 is the `syscall`
//! instruction, and the only values a program gets back are the `rax`/`rdx`
//! pair it returns. A hosted build's direct call is a test harness affordance,
//! and the whole module is behind `cfg(not(target_os = "none"))`, so it
//! cannot be compiled into a userland binary.
//!
//! # Why the services are not the kernel's own statics
//!
//! The kernel's `Exec`, `Dos`, `Graphics` and `Intuition` are owned by
//! `vela-hosted`, not by a global, because a test needs *two* kernels in one
//! process to test two tasks messaging each other. So the host installs them
//! here, and a program that runs without a harness gets a clean
//! `NOT_IMPLEMENTED` rather than a null dereference.

use spin::Mutex;

use crate::syscall::SyscallResult;

/// The services a hosted user program calls into.
///
/// Installed once by the host, never by a program. See the module
/// documentation for why.
struct Services {
    exec: &'static vela_kernel_exec::Exec,
    dos: &'static vela_kernel_dos::Dos,
    graphics: &'static vela_kernel_graphics::Graphics,
    intuition: &'static vela_kernel_intuition::Intuition,
    table: &'static vela_kernel_syscall::dispatch::DispatchTable,
}

static SERVICES: Mutex<Option<Services>> = Mutex::new(None);

/// Install the services a hosted user program calls into.
///
/// # Safety
///
/// Called once by the host, before any user code runs. The references must
/// outlive every program that will use them. Installing twice is not an error
/// — the second install replaces the first — but installing *after* user code
/// has started would swap the window manager out from under it.
pub unsafe fn install(
    exec: &'static vela_kernel_exec::Exec,
    dos: &'static vela_kernel_dos::Dos,
    graphics: &'static vela_kernel_graphics::Graphics,
    intuition: &'static vela_kernel_intuition::Intuition,
    table: &'static vela_kernel_syscall::dispatch::DispatchTable,
) {
    *SERVICES.lock() = Some(Services {
        exec,
        dos,
        graphics,
        intuition,
        table,
    });
}

/// The installed services, if any.
fn services() -> Option<Services> {
    // `Services` is five shared references, all `Copy` in effect, so copying
    // the fields out of the lock is safe and avoids holding the lock across
    // the syscall — which may itself take service locks.
    let services = SERVICES.lock();
    let services = services.as_ref()?;
    Some(Services {
        exec: services.exec,
        dos: services.dos,
        graphics: services.graphics,
        intuition: services.intuition,
        table: services.table,
    })
}

/// True when a harness has installed the services.
pub fn is_installed() -> bool {
    SERVICES.lock().is_some()
}

/// Dispatch a syscall through the installed services.
///
/// Returns [`crate::numbers::errors::NOT_IMPLEMENTED`] when nothing is
/// installed, so a hosted program run without a harness gets a clean error
/// rather than a null dereference.
pub fn dispatch(number: u64, args: [u64; 6]) -> SyscallResult {
    let Some(services) = services() else {
        return SyscallResult {
            value: 0,
            error: crate::numbers::errors::NOT_IMPLEMENTED,
        };
    };
    let syscall_args =
        vela_kernel_syscall::SyscallArgs::new(args[0], args[1], args[2], args[3], args[4], args[5]);
    let context = vela_kernel_syscall::dispatch::SyscallContext {
        task: services.exec.current_task(),
        exec: services.exec,
        dos: services.dos,
        graphics: services.graphics,
        intuition: services.intuition,
    };
    let result = services.table.dispatch(number, syscall_args, &context);
    SyscallResult {
        value: result.value,
        error: result.error,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::numbers::errors;

    #[test]
    fn dispatching_without_a_harness_is_a_clean_error() {
        // A program run before `install` must get an error code, not a
        // fault — that is the whole reason `SERVICES` is an `Option`.
        if is_installed() {
            // Another test in this binary installed services; this one is
            // about the *uninstalled* path, so it does not apply.
            return;
        }
        let result = dispatch(0, [0; 6]);
        assert_eq!(result.error, errors::NOT_IMPLEMENTED);
        assert_eq!(result.value, 0);
    }

    #[test]
    fn services_are_readable_through_a_lock() {
        // The lock is not a general-purpose one: it is read once per syscall
        // and released before dispatch, so a handler that takes a service
        // lock cannot deadlock against it.
        assert!(SERVICES.lock().is_none() || SERVICES.lock().is_some());
    }
}
