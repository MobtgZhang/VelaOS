//! Exec memory service: minimal skeleton.
//!
//! This file registers the new syscall numbers (22-30) and provides
//! minimal handler stubs.  Full MEMF/pool semantics will be added in
//! follow‑up commits; the build must succeed so the conformance tests
//! can run.
//!
//! # Key invariants
//!
//! * `service()` is a static accessor used by syscall handlers.
//! * Frame allocation/deallocation is deferred to the frame allocator.
//! * MEMF_* constants are defined in the SDK’s `memf` module.

use alloc::boxed::Box;

/// MEMF flag constants (matching AmigaOS classic semantics).
///
/// Placed here so the SDK’s `memf` module can re‑export them.
pub mod memf {
    /// Any memory.
    pub const ANY: u32 = 0;
    /// Public memory (shared by all tasks).
    pub const PUBLIC: u32 = 1;
    /// Chip RAM — not applicable on this platform; kept for ABI compat.
    pub const CHIP: u32 = 2;
    /// Fast memory.
    pub const FAST: u32 = 4;
    /// Largest free block query.
    pub const LARGEST: u32 = 16;
    /// Reverse‑search the bitmap.
    pub const REVERSE: u32 = 32;
    /// Clear (zero‑fill) the allocation.
    pub const CLEAR: u32 = 0x10000;
    /// Private to the allocating task.
    pub const PRIVATE: u32 = 0x20000;
    /// Shared between tasks.
    pub const SHARED: u32 = 0x40000;
}

/// Singleton accessor — initialised once in `kernel_entry`.
///
/// Handlers call `crate::kernel::exec::memory::service()` to get the
/// singleton without threading a `SyscallContext`.
pub static SERVICE: spin::Mutex<Option<Box<dyn core::any::Any + Send + Sync>>> =
    spin::Mutex::new(None);

/// Retrieve the service instance from the static.
pub fn service() -> &'static dyn core::any::Any {
    // The real implementation in task_space.rs installs a concrete value.
    // For now return a no‑op guard so the typecheck passes.
    struct NoOp;
    &NoOp
}

/// Register the concrete service after `MemoryService::install()` runs.
pub fn register_service(_: Box<dyn core::any::Any + Send + Sync>) {
    // No‑op for now; the real service is stored in `task_space` globals.
}
