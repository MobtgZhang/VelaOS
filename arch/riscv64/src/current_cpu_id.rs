//! The per-hart identifier, as the boot protocol delivered it.
//!
//! # Why a whole module for one function
//!
//! Surface parity, again: the x86-64 crate exposes `current_cpu_id` at both
//! the module and the crate root, and the facade re-exports it. On RISC-V
//! the value comes from the boot protocol rather than a register read, and
//! the full reasoning lives in [`crate::regs::hart_id`]; this module exists
//! so the kernel can `use vela_arch_riscv64::current_cpu_id` the same way it
//! does for x86-64, with the *why* documented in exactly one place.
//!
//! The short version: `mhartid` is M-mode-only (an S-mode read raises an
//! illegal-instruction exception), and no SBI function exposes the hart id,
//! so OpenSBI's boot convention — the hart's id in `a0` when the kernel's
//! entry runs — is the only source. The kernel entry records it once per
//! hart with [`crate::regs::set_boot_hartid`] before anything reads it.

pub use crate::regs::{hart_id, set_boot_hartid};
