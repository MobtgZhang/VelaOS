//! gui: the architecture-neutral GUI facade.
//!
//! # What this crate is
//!
//! It is the aggregation point for the Amiga-style GUI subsystem: the
//! `graphics.library` service, the `layers.library` compositor, the
//! `intuition.library` window manager, and the BOOPSI object model.
//!
//! The services themselves live in their own crates because they have
//! different owners — graphics owns pens and palettes, layers owns damage and
//! z-order, intuition owns screens and windows — and each is unit-tested on
//! its own. This crate re-exports them under the names a program expects and
//! keeps BOOPSI here, since BOOPSI is a small object model rather than a
//! window manager.
//!
//! There is deliberately no duplicate implementation. A second copy of the
//! graphics/layers/intuition services would be a second source of truth about
//! the same palette and layer state, which is exactly the kind of split the
//! kernel's syscall dispatch must not have.

#![no_std]

pub mod boopsi;

/// The graphics service (`graphics.library`).
pub use vela_kernel_graphics as graphics;
/// The window manager (`intuition.library`).
pub use vela_kernel_intuition as intuition;
/// The layer compositor (`layers.library`).
pub use vela_kernel_layers as layers;
/// The Workbench object registry (`workbench.library`).
pub use vela_kernel_workbench as workbench;
