//! The x86-64 syscall boundary: the `syscall` gate and ring-3 entry.
//!
//! The architecture crate owns the *mechanism* — the assembly entry stub, the
//! MSR setup, the selector arithmetic, and the `iretq` frame used to enter
//! ring 3. The kernel owns the *policy* — the dispatch table and the handler
//! that runs on each syscall. The gate therefore calls a kernel-provided
//! dispatch function, registered through [`gate::install`].

pub mod gate;
pub mod userspace;
