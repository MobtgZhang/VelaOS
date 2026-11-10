//! The architecture facade.
//!
//! High-level kernel code imports hardware operations from this crate, not
//! from a concrete architecture crate. The facade selects the implementation
//! for the target triple at build time. On x86-64 it re-exports
//! `vela-arch-x86_64`; the other architectures are compile-checked through the
//! common trait surface and their own placeholder crates in `arch/*`.
//!
//! # Why a facade instead of direct imports
//!
//! A crate named `vela-arch-x86_64` in `kernel/scheduler` would make the
//! scheduler x86-only by construction. The facade keeps the import
//! architecture-neutral at the source level and confines the target switch to
//! one module. Assembly, page-table layout, IPIs and register contracts live
//! in the architecture crates under `arch/`.

#![no_std]

pub use vela_arch_common::*;

#[cfg(target_arch = "x86_64")]
pub use vela_arch_x86_64::{
    addrspace, apic, context, current_cpu_id, gdt, idt, paging, pci, ps2, qemu_exit, regs, serial,
    smp, syscall, userspace,
};

#[cfg(target_arch = "x86_64")]
pub use vela_arch_x86_64::{AddressSpace, FrameSource, PhysOfVirt};

// The other three architectures carry their own layers: paging, the context
// switch, the syscall gate, the interrupt controller, the timer and the
// console, each written for that instruction set. The facade re-exports the
// same module names for all of them, so the code above the facade never has
// to know which one it is running on.
#[cfg(target_arch = "aarch64")]
pub use vela_arch_aarch64::{
    addrspace, apic, context, current_cpu_id, gdt, idt, paging, pci, ps2, qemu_exit, regs, serial,
    smp, syscall, userspace,
};

#[cfg(target_arch = "aarch64")]
pub use vela_arch_aarch64::{AddressSpace, FrameSource, PhysOfVirt};

#[cfg(target_arch = "riscv64")]
pub use vela_arch_riscv64::{
    addrspace, apic, context, current_cpu_id, gdt, idt, paging, pci, ps2, qemu_exit, regs, serial,
    smp, syscall, userspace,
};

#[cfg(target_arch = "riscv64")]
pub use vela_arch_riscv64::{AddressSpace, FrameSource, PhysOfVirt};

#[cfg(target_arch = "loongarch64")]
pub use vela_arch_loongarch64::{
    addrspace, apic, context, current_cpu_id, gdt, idt, paging, pci, ps2, qemu_exit, regs, serial,
    smp, syscall, userspace,
};

#[cfg(target_arch = "loongarch64")]
pub use vela_arch_loongarch64::{AddressSpace, FrameSource, PhysOfVirt};

/// True when this build has a real architecture layer behind the facade.
///
/// The placeholder modules below are what an architecture without a layer
/// compiles against. A build that has one — every architecture the project
/// supports — reports `true`, so a caller can tell the difference between
/// "this architecture has no layer yet" and "this architecture's layer is
/// present".
pub const HAS_ARCHITECTURE_LAYER: bool = cfg!(any(
    target_arch = "x86_64",
    target_arch = "aarch64",
    target_arch = "riscv64",
    target_arch = "loongarch64"
));

#[cfg(not(any(
    target_arch = "x86_64",
    target_arch = "aarch64",
    target_arch = "riscv64",
    target_arch = "loongarch64"
)))]
pub mod addrspace {
    //! A placeholder page-table type for compile-check targets.
    //!
    //! The real aarch64/riscv64/loongarch64 implementations are future
    //! milestones. This type exists so the facade has a stable surface and the
    //! non-x86 crates compile without pretending to boot.
    use vela_arch_common::{AddressSpaceId, MapError, PageFlags, PAGE_SIZE};

    pub trait PhysOfVirt {
        fn phys_of(&self, virt: *const u8) -> u64;
    }

    #[derive(Debug, Clone, Copy, Default)]
    pub struct IdentityPhys;

    impl PhysOfVirt for IdentityPhys {
        fn phys_of(&self, virt: *const u8) -> u64 {
            virt as u64
        }
    }

    pub trait FrameSource {
        fn alloc_frame(&mut self) -> Result<u64, MapError>;
        unsafe fn free_frame(&mut self, pa: u64);
    }

    /// An architecture-neutral placeholder. It performs no mapping, because a
    /// compile-check target has no running page-table implementation yet.
    pub struct AddressSpace {
        id: AddressSpaceId,
    }

    impl AddressSpace {
        pub fn new(id: AddressSpaceId) -> Self {
            Self { id }
        }

        pub fn with_kernel_mappings(id: AddressSpaceId, _kernel: &AddressSpace) -> Self {
            Self { id }
        }

        pub const fn id(&self) -> AddressSpaceId {
            self.id
        }

        pub const fn cr3(&self) -> u64 {
            self.id.0
        }

        pub fn set_pml4_base(&mut self, pml4_pa: u64) {
            self.id = AddressSpaceId(pml4_pa);
        }

        pub fn mapped_frames(&self) -> usize {
            0
        }

        pub fn is_mapped(&self, _va: u64) -> bool {
            false
        }

        pub fn map_page(&self, va: u64, pa: u64, _flags: PageFlags) -> Result<(), MapError> {
            if !va.is_multiple_of(PAGE_SIZE) {
                return Err(MapError::MisalignedVirtual);
            }
            if !pa.is_multiple_of(PAGE_SIZE) {
                return Err(MapError::MisalignedPhysical);
            }
            Err(MapError::OutOfMemory)
        }

        pub fn map_range(
            &self,
            va: u64,
            pa: u64,
            len: usize,
            flags: PageFlags,
        ) -> Result<(), MapError> {
            if len == 0 || !len.is_multiple_of(PAGE_SIZE as usize) {
                return Err(MapError::MisalignedVirtual);
            }
            for i in 0..(len as u64 / PAGE_SIZE) {
                self.map_page(va + i * PAGE_SIZE, pa + i * PAGE_SIZE, flags)?;
            }
            Ok(())
        }

        pub fn unmap_page(&self, _va: u64) -> Result<u64, MapError> {
            Err(MapError::NotMapped)
        }

        pub fn unmap_range(&self, _va: u64, _len: usize) -> Result<alloc::vec::Vec<u64>, MapError> {
            Err(MapError::NotMapped)
        }

        pub fn set_flags(&self, _va: u64, _flags: PageFlags) -> Result<(), MapError> {
            Err(MapError::NotMapped)
        }

        pub fn translate(&self, _va: u64) -> Option<u64> {
            None
        }

        pub fn pml4_ptr(&self) -> *const u8 {
            core::ptr::null()
        }

        pub fn pml4_pa(&self) -> u64 {
            self.id.0
        }

        pub fn summary(&self) -> alloc::vec::Vec<alloc::string::String> {
            alloc::vec::Vec::new()
        }

        pub fn teardown(&mut self) {}
    }

    pub unsafe fn activate(_space: &AddressSpace) {}
}

#[cfg(not(any(
    target_arch = "x86_64",
    target_arch = "aarch64",
    target_arch = "riscv64",
    target_arch = "loongarch64"
)))]
pub mod context {
    //! A placeholder context type for compile-check targets.
    #[derive(Debug, Clone, Copy)]
    #[repr(C)]
    pub struct Context {
        pub rsp: u64,
        pub rbp: u64,
        pub rbx: u64,
        pub r12: u64,
        pub r13: u64,
        pub r14: u64,
        pub r15: u64,
        pub xmm: [u128; 10],
    }

    impl Context {
        pub const fn new() -> Self {
            Self {
                rsp: 0,
                rbp: 0,
                rbx: 0,
                r12: 0,
                r13: 0,
                r14: 0,
                r15: 0,
                xmm: [0; 10],
            }
        }

        pub fn for_entry(stack_top: u64, entry: u64, arg: u64) -> Self {
            let aligned = stack_top & !0xF;
            Self {
                rsp: aligned - 16,
                rbp: 0,
                rbx: 0,
                r12: entry,
                r13: arg,
                r14: 0,
                r15: 0,
                xmm: [0; 10],
            }
        }

        pub const fn stack_pointer(&self) -> u64 {
            self.rsp
        }
    }

    pub unsafe fn switch_to(_current: *mut Context, _next: *const Context) {}
    pub unsafe fn task_entry_trampoline() -> ! {
        loop {}
    }
}

#[cfg(not(any(
    target_arch = "x86_64",
    target_arch = "aarch64",
    target_arch = "riscv64",
    target_arch = "loongarch64"
)))]
pub mod serial {
    pub fn puts(_s: &str) {}
    pub fn puts_hex(_v: u64) {}
    pub fn puts_dec(_v: u64) {}
}

#[cfg(not(any(
    target_arch = "x86_64",
    target_arch = "aarch64",
    target_arch = "riscv64",
    target_arch = "loongarch64"
)))]
pub mod qemu_exit {
    pub fn power_off(_code: u32) -> ! {
        loop {}
    }
}

#[cfg(not(any(
    target_arch = "x86_64",
    target_arch = "aarch64",
    target_arch = "riscv64",
    target_arch = "loongarch64"
)))]
pub mod regs {
    pub const ON_BARE_METAL: bool = false;
}

extern crate alloc;

#[cfg(test)]
extern crate std;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_common_surface_is_always_available() {
        let flags = PageFlags::user_rw();
        assert!(flags.user_writable);
        assert!(!flags.is_wx_violation());
    }

    #[test]
    fn the_facade_selects_the_target_implementation() {
        // On x86-64 the concrete AddressSpace is the x86 implementation; on a
        // compile-check target the placeholder still has the same public shape.
        let space = AddressSpace::new(AddressSpaceId(0x1000));
        assert_eq!(space.id().0, 0x1000);
    }
}
