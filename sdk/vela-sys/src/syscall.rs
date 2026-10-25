//! The `syscall` instruction, as user space sees it.
//!
//! # One instruction, two results
//!
//! A VelaOS syscall returns a **value** in `rax` and an **error code** in
//! `rdx`, not a single negative return value. The reason is that the AmigaOS
//! API returns a pointer on success and NULL on failure, and a NULL return is
//! a legitimate result for some calls — `OpenLibrary` for a library that is
//! not there, `OpenScreen` with an impossible geometry. A single-return-value
//! convention cannot say "succeeded, and the answer was NULL", so VelaOS
//! splits them and this module hands both back.
//!
//! # Why `#[cfg]` on the instruction
//!
//! The `syscall` instruction exists only on real hardware. Under a hosted
//! test — which is how every piece of kernel logic in this repository is
//! exercised — there is no ring 3 and no gate, so the instruction would
//! fault. The hosted path therefore calls the kernel's dispatcher directly,
//! which is the same code the gate calls; only the transition is substituted.
//!
//! That substitution is what makes the boundary testable at all, and it is
//! also why the boundary's *hardening* is tested separately: the user-copy
//! helpers are exercised against real addresses, including bad ones.

/// The six argument registers of the VelaOS syscall ABI.
///
/// # Why `r10` rather than `rcx`
///
/// The `syscall` instruction itself clobbers `rcx` and `r11` — it puts the
/// return address in `rcx` and the saved `RFLAGS` in `r11`. So neither can be
/// an argument register, and the System V sequence `rdi, rsi, rdx, rcx, r8, r9`
/// becomes `rdi, rsi, rdx, r10, r8, r9`.
///
/// # Safety
///
/// The caller must already be running in ring 3, must own the pointers passed
/// in `args`, and must not rely on this function to validate them.
#[inline]
pub unsafe fn syscall_raw(number: u64, args: [u64; 6]) -> SyscallResult {
    let a0 = args[0];
    let a1 = args[1];
    let a2 = args[2];
    let a3 = args[3];
    let a4 = args[4];
    let a5 = args[5];
    let mut value: u64;
    let mut error: u64;

    // SAFETY: this is the whole point of the module. The assembly matches
    // `kernel/syscall/src/gate.rs`'s expectation exactly: the number in `rax`,
    // six arguments in `rdi, rsi, rdx, r10, r8, r9`, and a result in `rax`
    // and `rdx`. It clobbers `rcx` and `r11`, which is why they are not
    // argument registers — `gate.rs` relies on `rcx` and `r11` surviving the
    // handler for `sysretq`.
    unsafe {
        core::arch::asm!(
            "syscall",
            inlateout("rax") number => value,
            inlateout("rdx") a2 => error,
            in("rdi") a0,
            in("rsi") a1,
            in("r10") a3,
            in("r8") a4,
            in("r9") a5,
            lateout("rcx") _,
            lateout("r11") _,
        );
    }

    SyscallResult {
        value,
        error: error as i32,
    }
}

/// Issue a VelaOS syscall.
///
/// # Safety
///
/// `number` must be a syscall this program is allowed to make, and any
/// argument that is a pointer must point at memory the program owns and has
/// written. The kernel validates pointers itself — a bad one is an error, not
/// a fault — so this is a correctness obligation on the *program's* side, not
/// a memory-safety one.
#[inline]
pub unsafe fn syscall(number: u64, args: [u64; 6]) -> SyscallResult {
    #[cfg(target_os = "none")]
    {
        syscall_raw(number, args)
    }
    #[cfg(not(target_os = "none"))]
    {
        crate::hosted::dispatch(number, args)
    }
}

/// The result of a call, as user space sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SyscallResult {
    /// The value the call returned.
    pub value: u64,
    /// The error code, or zero on success.
    pub error: i32,
}

impl SyscallResult {
    /// True when the call succeeded.
    pub const fn is_ok(self) -> bool {
        self.error == 0
    }

    /// True when the call failed.
    pub const fn is_err(self) -> bool {
        self.error != 0
    }

    /// The value, or `fallback` on failure.
    ///
    /// `fallback` rather than zero because zero is a legitimate return value:
    /// `CloseLibrary` returns 0 on success, and a program that cannot tell
    /// the two apart would report a successful close as a failure.
    pub const fn value_or(self, fallback: u64) -> u64 {
        if self.error == 0 {
            self.value
        } else {
            fallback
        }
    }
}
