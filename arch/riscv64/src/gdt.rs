//! There is no GDT on RISC-V.
//!
//! # Why this module exists
//!
//! The x86-64 facade re-exports `gdt` because segment descriptors are load-
//! bearing there: `KERNEL_CODE_SELECTOR` names a GDT entry, every IDT gate
//! carries it, and `iretq`/`far return` consume it. RISC-V has no segment
//! registers and no descriptor tables at all — privilege is carried in
//! `sstatus.SPP` per trap and chosen per `sret`, and the psABI knows no
//! selectors. A kernel build for riscv64 therefore has *nothing* to install,
//! and this module ships no code on purpose.
//!
//! The x86-64 module surface is mirrored name-for-name (see the crate docs)
//! so that a facade switch stays a one-line change; a future contributor who
//! finds this file empty should leave it empty. The only RISC-V concept that
//! even *resembles* segmentation is the PMP (Physical Memory Protection),
//! whose CSRs are M-mode-only — see [`crate::paging`]'s PMP section for why
//! that boundary is the firmware's business and not the kernel's.
//!
//! # What replaced it, in one table
//!
//! | x86-64 concept | RISC-V replacement |
//! |---|---|
//! | GDT code/data descriptors | none — privilege is `SPP`, not a selector |
//! | `KERNEL_CODE_SELECTOR` | none — `jalr`/`sret` need no selector |
//! | `cli`/`sti` via `IF` | `sstatus.SIE` masking ([`crate::regs`]) |
//! | GDT-based per-CPU (`gs`/`fs`) | `tp` + `sscratch` ([`crate::trap`]) |
