//! The loongarch64 architecture surface.
//!
//! Compile-check only in v0.1.0. The common CPU trait is implemented with
//! placeholder operations; a future milestone uses `idle`, per-CPU `$sp`/`$tp`
//! conventions, interrupt masking and LoongArch page tables.

#![no_std]

use core::sync::atomic::{AtomicUsize, Ordering};

use vela_arch_common::{AddressSpaceId, CpuArch, MapError, PageFlags};

/// The loongarch64 CPU identity used by the compile-check harness.
pub struct LoongarchCpu;

static THIS_CPU: AtomicUsize = AtomicUsize::new(0);

impl CpuArch for LoongarchCpu {
    fn halt() {}
    fn enable_interrupts() {}
    fn disable_interrupts() {}
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
    unsafe fn register_cpu(_cpu_id: u32, index: usize) {
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
