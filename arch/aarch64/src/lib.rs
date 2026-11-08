//! The aarch64 architecture surface.
//!
//! This crate is compile-check only in v0.1.0. It implements the common CPU
//! trait without machine instructions so the architecture-neutral kernel crates
//! can be checked for portability. A future milestone replaces the placeholder
//! operations with real `DAIF` masking, `WFI`, per-CPU registers and page-table
//! code.

#![no_std]

use core::sync::atomic::{AtomicUsize, Ordering};

use vela_arch_common::{AddressSpaceId, CpuArch, MapError, PageFlags};

/// The aarch64 CPU identity used by the compile-check harness.
pub struct Aarch64Cpu;

static THIS_CPU: AtomicUsize = AtomicUsize::new(0);

impl CpuArch for Aarch64Cpu {
    fn halt() {
        // Placeholder: a real implementation uses WFI.
    }

    fn enable_interrupts() {
        // Placeholder: a real implementation clears the DAIF interrupt mask.
    }

    fn disable_interrupts() {
        // Placeholder: a real implementation sets the DAIF interrupt mask.
    }

    fn interrupts_enabled() -> bool {
        false
    }

    fn current_cpu_id() -> usize {
        THIS_CPU.load(Ordering::Acquire)
    }

    fn busy_wait_cycles() -> u64 {
        0
    }

    unsafe fn set_this_cpu(index: usize) {
        THIS_CPU.store(index, Ordering::Release);
    }

    unsafe fn register_cpu(_apic_id: u32, index: usize) {
        THIS_CPU.store(index, Ordering::Release);
    }

    fn mark_cpu_ready() {}

    fn is_cpu_ready() -> bool {
        false
    }

    fn set_current_task(_task_id: u32) {}

    fn current_task_id() -> u32 {
        0
    }
}

/// A placeholder address space for non-booting compile checks.
pub struct AddressSpace {
    #[allow(dead_code)]
    id: AddressSpaceId,
}

impl AddressSpace {
    pub fn new(id: AddressSpaceId) -> Self {
        Self { id }
    }

    pub fn map_page(&self, _va: u64, _pa: u64, _flags: PageFlags) -> Result<(), MapError> {
        Err(MapError::OutOfMemory)
    }
}
