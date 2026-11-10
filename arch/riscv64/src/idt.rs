//! The trap table — named after the x86-64 `idt` for surface parity.
//!
//! # Why this module is an alias
//!
//! x86-64 keeps its exception vectors in an IDT, a table of gate descriptors
//! that the CPU indexes by vector number. RISC-V has no such table: `stvec`
//! names *one* entry point, and the cause demultiplexing happens in software
//! inside [`crate::trap`]'s entry stub. There is nothing to build, register
//! or gate — the "table" is the stub's branch tree, and the frame layout and
//! `scause` decoder are the whole of the mechanism.
//!
//! The x86-64 facade re-exports this module name, so the kernel can talk
//! about "the IDT" in architecture-neutral code; for riscv64 that name
//! resolves to the trap machinery, which is the same role. Kernel code
//! should still prefer the canonical names in [`crate::trap`].

pub use crate::trap::*;
