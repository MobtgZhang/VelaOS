//! The interrupt controller — named after the x86-64 `apic` for parity.
//!
//! # Why this module is an alias
//!
//! x86-64's external-interrupt controller is the APIC pair (local + I/O
//! APIC); RISC-V's is the PLIC (Platform-Level Interrupt Controller), and
//! the real implementation lives in [`crate::plic`] — the register map, the
//! context arithmetic and the claim/complete protocol.
//!
//! On RISC-V there is nothing that plays the local APIC's *other* roles:
//! there is no IPI register set in S-mode (S-mode cannot send IPIs at all
//! without an SBI extension or an M-mode helper), and the timer is CLINT +
//! `stimecmp`/SBI rather than an LVT timer — see [`crate::timer`]. So this
//! alias deliberately re-exports only the interrupt-routing surface; the
//! timer surface is a separate module, as on x86-64 where `pit` is separate
//! from `apic`.

pub use crate::plic::*;
