//! The LoongArch64 syscall boundary: the `SYS` exception gate and ring-3
//! entry.
//!
//! The architecture crate owns the *mechanism* — the register convention, the
//! `PRMD`/`CRMD` builders, and the `ertn` return path. The kernel owns the
//! *policy* — the dispatch table and the handler that runs on each syscall.
//! The gate therefore calls a kernel-provided dispatch function, registered
//! through [`gate::install`].
//!
//! # The one architectural difference from x86-64
//!
//! x86-64's `syscall` bypasses the IDT entirely and jumps to a dedicated
//! `LSTAR` entry point, so the x86 crate owns a private assembly stub. On
//! LoongArch64 the `syscall` instruction is *just an exception*: it enters at
//! `EENTRY + EC × VECSIZE` with `EC = 11` (`SYS`, see [`crate::idt`]), having
//! set `CRMD.PLV = 0`, `CRMD.IE = 0`, saved the old mode in `PRMD`, and put
//! the restart PC in `ERA`. The single trap stub in [`crate::idt`] therefore
//! handles syscalls — stack switch, register save, dispatch — exactly like
//! any other exception. This module has no assembly of its own.

pub mod gate;
pub mod userspace;
