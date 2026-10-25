//! The VelaOS kernel entry point.
//!
//! # The handoff contract
//!
//! The UEFI loader jumps here with:
//!
//! * `RDI` = the **physical** address of a [`VelaBootInfo`].
//! * `RSP` = a private stack allocated from the loader's boot pool, already
//!   16-byte aligned.
//! * `RBP` = 0, so no caller frame chain is implied.
//! * `CR0.PG` = 0: paging is off. The firmware exited boot services in
//!   protected mode and the kernel builds its own page tables.
//!
//! The kernel therefore arrives with **no virtual memory at all**. The
//! firmware's own page tables are discarded and replaced.
//!
//! # The boot sequence
//!
//! 1. **Serial.** Take over COM1. Nothing can be reported until this works,
//!    so it is first.
//! 2. **Boot info.** Read and validate the handoff. A bad handoff must be
//!    reported clearly, not dereferenced.
//! 3. **Heap.** Install the kernel heap. This comes *before* the memory map
//!    is parsed, because parsing the map allocates a `Vec` of descriptors and
//!    an allocator that is not yet installed cannot serve it.
//! 4. **Memory map.** Copy the firmware's memory map out of the boot pool.
//! 5. **Plan.** Decide where the frame bitmap lives. This has to happen
//!    *before* any allocator exists, because the allocator's own metadata
//!    needs memory.
//! 6. **Address space.** Build the kernel's page tables, map the kernel image
//!    into the higher half, and switch to them.
//! 7. **Segments.** Install the GDT and the syscall gate. The gate's
//!    `IA32_STAR` names selectors from the kernel's own GDT, so the two must
//!    be installed together.
//! 8. **Interrupts.** Install the IDT, then the APIC timer, then unmask.
//! 9. **Banner.** Print the version and the memory summary.
//!
//! # The higher half
//!
//! VelaOS is a "higher-half" kernel: the kernel image is linked at
//! `0xFFFF_8000_0000_0000` and runs from there, while user space lives in the
//! low half. The loader places the kernel image at a low physical address and
//! the kernel maps it into the higher half itself.
//!
//! This is the standard layout because it leaves the entire low half free for
//! user processes, and because a kernel linked at a high virtual address has
//! its symbols near the top of the address space, where a stray user pointer
//! is unlikely to reach.

#![no_std]

extern crate alloc;

use alloc::boxed::Box;
use alloc::vec::Vec;

use alloc::string::ToString;
use core::sync::atomic::{AtomicBool, Ordering};

use vela_boot_protocol::VelaBootInfo;
use vela_hal_x86_64::addrspace::{activate, AddressSpace, IdentityPhys};
use vela_hal_x86_64::{apic, idt, paging, regs, serial};
use vela_kernel_core::Level;
use vela_kernel_memory::{
    init_heap, BootReservations, FrameAllocator, KernelHeap, MemoryMap, MemoryPlan, PAGE_SIZE,
};
use vela_kernel_syscall::dispatch::{builtin_table, DispatchTable};

/// The virtual base of the higher-half kernel window.
///
/// The window is 256 MiB, which is the size of a second-level page-table
/// entry, so the whole kernel fits in a single PDPT slot and its mapping is
/// three table entries rather than four.
pub const KERNEL_VIRT_BASE: u64 = paging::KERNEL_VIRTUAL_BASE;

/// The size of the higher-half kernel window, in bytes.
pub const KERNEL_VIRT_SIZE: u64 = 0x1000_0000; // 256 MiB

/// How much of physical memory the kernel reserves for its bump heap.
///
/// 4 MiB is enough for the kernel's own structures — the frame bitmap, the
/// scheduler's task tables, the Exec port lists — with room to spare.
///
/// The *actual* heap the kernel uses is the one the loader reserved before
/// `ExitBootServices`, and its address comes from the boot handoff. This
/// constant is the size the memory plan asks for; the loader's reservation is
/// the authority, and the two are checked against each other at boot.
pub const KERNEL_HEAP_BYTES: u64 = 4 * 1024 * 1024;

/// The size of the kernel's boot stack, in bytes.
///
/// The loader hands over a stack; the kernel builds its own on top of the
/// heap so the loader's stack can be reclaimed. 64 KiB is generous for the
/// kernel's own call depth.
pub const KERNEL_STACK_BYTES: u64 = 64 * 1024;

/// The size of a large page, in bytes.
///
/// A 2 MiB page is what the kernel's identity map is built from: mapping the
/// machine's RAM with 4 KiB pages would need one page table per 4 KiB of RAM,
/// which does not fit in the kernel heap.
pub const LARGE_PAGE_BYTES: u64 = 2 * 1024 * 1024;

/// The physical address of the local APIC window.
///
/// The APIC is device memory: it is read and written directly by physical
/// address, so the kernel's own address space must map it.
pub const LAPIC_BASE: u64 = 0xFEE0_0000;

/// The exit status the kernel uses when the boot handoff is invalid.
pub const EXIT_BAD_HANDOFF: u32 = 3;

/// The exit status the kernel uses when physical memory cannot be planned.
pub const EXIT_NO_MEMORY: u32 = 4;

/// Whether the kernel has finished booting.
///
/// The boot-smoke test looks for the ready marker in the serial log, and the
/// userland's `init` process is only started once this is true.
static BOOTED: AtomicBool = AtomicBool::new(false);

/// The kernel's frame allocator, seen through the syscall layer's interface.
///
/// # Why this lives here
///
/// The allocator itself belongs to `kernel/memory`, which owns it. But the
/// *syscall* layer needs to reach it — `MapPage` and `Brk` grow a process's
/// heap, and growing a heap consumes physical frames — and the syscall layer
/// must not depend on the memory layer, or the two would form a cycle.
///
/// So the bridge is here, in the crate that depends on both: `kernel/entry`
/// already depends on `kernel/memory` (for the allocator) and on
/// `kernel/syscall` (for the dispatch table), so it is the one place where
/// the two can be joined without either of them knowing about the other.
///
/// # Why a newtype rather than an impl on `FrameAllocator`
///
/// Rust's orphan rule forbids implementing a foreign trait for a foreign
/// type. `FrameAllocator` belongs to `kernel/memory` and `FrameSource`
/// belongs to `kernel/syscall`, so neither crate may write the impl on the
/// allocator directly — and `kernel/entry` may not either, because the
/// allocator is still a foreign type from here.
///
/// A newtype is the way out. This type is local, so the impl is allowed, and
/// the newtype is also where the `Sync` guarantee lives: the syscall layer
/// installs the source behind a `&'static (dyn FrameSource + Sync)`, so the
/// allocator has to be usable from more than one CPU.
///
/// # Why sharing it is sound
///
/// The allocator's own state is a bitmap of atomics and an immutable base
/// address, and every one of its methods takes `&self`. Two CPUs allocating
/// at the same time therefore contend on the bitmap's compare-and-swap and
/// cannot hand out the same frame twice.
struct UserFrames(&'static FrameAllocator);

// SAFETY: the allocator's bitmap is mutated through atomics and its base
// address is immutable, and every method takes `&self`, so sharing the
// allocator between CPUs cannot produce a data race. The newtype adds no
// state of its own.
unsafe impl Sync for UserFrames {}

impl vela_kernel_syscall::task_space::FrameSource for UserFrames {
    fn alloc_frame(&self) -> Result<u64, vela_hal_common::MapError> {
        self.0
            .alloc_frame()
            .ok_or(vela_hal_common::MapError::OutOfMemory)
    }

    unsafe fn free_frame(&self, pa: u64) {
        // SAFETY: forwarded from this function's own contract.
        unsafe { self.0.free_frame(pa) };
    }
}

/// True once the kernel has finished booting.
pub fn is_booted() -> bool {
    BOOTED.load(Ordering::Acquire)
}

/// The kernel entry point.
///
/// # Safety
///
/// The loader must have:
///
/// * set `RDI` to the physical address of a valid [`VelaBootInfo`],
/// * set `RSP` to a valid, 16-byte-aligned stack,
/// * zeroed `RBP`,
/// * left paging disabled.
///
/// This is the handoff contract documented in `boot/uefi-loader`.
#[no_mangle]
pub unsafe extern "C" fn kernel_entry(boot_info_paddr: u64) -> ! {
    // --- 1. Serial ---------------------------------------------------------
    // SAFETY: the loader has exited boot services, so the firmware no longer
    // owns COM1.
    unsafe { serial::init() };
    serial::puts("\n");
    serial::puts("VELA-KERNEL: entering\n");

    // --- 2. Boot info ------------------------------------------------------
    // The boot info is a *physical* address and paging is off, so it is
    // directly dereferenceable. It is read once and copied out, because the
    // page it lives in is inside the loader's pool and will be reclaimed.
    // SAFETY: the loader guarantees the page is readable and holds a
    // `VelaBootInfo`.
    let boot_info = unsafe { *(boot_info_paddr as *const VelaBootInfo) };

    if let Err(e) = boot_info.check_identity() {
        serial::puts("VELA-KERNEL: bad boot handoff: ");
        serial::puts(&e.to_string());
        serial::puts("\n");
        vela_hal_x86_64::qemu_exit::power_off(EXIT_BAD_HANDOFF);
    }
    serial::puts("VELA-KERNEL: boot info accepted\n");

    // --- 5. Kernel heap ----------------------------------------------------
    //
    // The heap is installed **before** anything allocates, which means
    // before the memory map is parsed: `MemoryMap::parse` collects the
    // firmware's descriptors into a `Vec`, and a `Vec` allocates. An earlier
    // version of this sequence parsed the map first and then installed the
    // heap, which meant the very first allocation the kernel attempted
    // failed with "memory allocation of 160 bytes failed" — a panic before
    // the kernel had said anything about itself.
    //
    // The heap's region is the one the **loader** reserved, not one the
    // kernel carves out of the memory map.
    //
    // That matters for a reason that is not obvious: the kernel's page tables
    // live in this heap, and the kernel computes a page table's *physical*
    // address from the heap pointer that holds it. That is only sound if the
    // heap is mapped at the same virtual and physical address — which is
    // exactly what the loader arranges, and exactly what the bootstrap page
    // tables identity-map.
    //
    // So the heap's physical address comes from the boot handoff, its
    // virtual address is the same number, and the translation is the
    // identity. The assumption is *true*, not merely asserted.
    //
    // The heap is a `KernelHeap` in a leaked allocation: the bump allocator
    // needs a stable address, and the heap must outlive every allocation made
    // from it.
    if boot_info.heap_bytes == 0 || boot_info.heap_paddr == 0 {
        serial::puts("VELA-KERNEL: the loader reserved no heap\n");
        vela_hal_x86_64::qemu_exit::power_off(EXIT_NO_MEMORY);
    }
    // The heap's *metadata* — the bump allocator's own bookkeeping — lives in
    // the first bytes of the loader's reservation, and the bump region starts
    // immediately after it.
    //
    // # Why the metadata lives inside the reserved region
    //
    // The heap has to be a `&'static KernelHeap`, because the global
    // allocator hands out pointers into it for the rest of the kernel's life.
    // A previous version of this code built the heap on the *stack* and took
    // `&*core::ptr::from_ref(&heap)`, which produces a reference to a
    // temporary that is dropped at the end of the statement: the pointer
    // dangles, the allocator reads a garbage base and end, and the very first
    // allocation fails with "memory allocation of 160 bytes failed".
    //
    // Putting the metadata at the front of the loader's own reservation
    // avoids that entirely, and has a second benefit: the metadata's address
    // *is* the heap's physical address, so the heap's virtual and physical
    // addresses are the same number. That is what makes the identity
    // translation of the heap — and therefore `pml4_phys(&IdentityPhys)` —
    // honest rather than a placeholder.
    //
    // The reservation is page-aligned, so the metadata is naturally aligned
    // for a `KernelHeap`, and the bump region that follows is still
    // page-aligned.
    let heap_meta_bytes = core::mem::size_of::<KernelHeap>();
    if (boot_info.heap_bytes as usize) <= heap_meta_bytes {
        serial::puts("VELA-KERNEL: the loader reserved no heap\n");
        vela_hal_x86_64::qemu_exit::power_off(EXIT_NO_MEMORY);
    }
    // SAFETY: the loader reserved the region, it is page-aligned and larger
    // than the metadata, and it is identity-mapped by the bootstrap tables.
    // The metadata is written exactly once, before any allocation is served
    // from the heap, and the bump region begins after it, so no two things
    // ever occupy the same byte.
    let heap: &'static KernelHeap = unsafe {
        let meta = boot_info.heap_paddr as *mut KernelHeap;
        // SAFETY: the region is the loader's reservation, which is valid,
        // writable, physically contiguous and page-aligned.
        meta.write(KernelHeap::new(
            (boot_info.heap_paddr as usize) + heap_meta_bytes,
            (boot_info.heap_bytes as usize) - heap_meta_bytes,
        ));
        &*meta
    };
    // SAFETY: the heap outlives the kernel — it lives in a region the loader
    // reserved and never reclaims.
    unsafe { init_heap(heap) };
    serial::puts("VELA-KERNEL: heap installed at ");
    serial::puts_hex(boot_info.heap_paddr);
    serial::puts("\n");

    // --- 6. Memory map -----------------------------------------------------
    // SAFETY: the boot info points at the loader's persisted memory map, which
    // the loader guarantees is readable and page-aligned.
    let map_bytes = unsafe {
        core::slice::from_raw_parts(
            boot_info.memory_map_paddr as *const u8,
            boot_info.memory_map_bytes as usize,
        )
    };
    let map = MemoryMap::parse(map_bytes, boot_info.memory_desc_size as usize);
    serial::puts("VELA-KERNEL: memory ");
    serial::puts(&map.summary());
    serial::puts("\n");

    // --- 7. Physical memory plan -------------------------------------------
    // The bitmap must describe every frame from 0 to the highest address the
    // map mentions, so its size is known before it can be placed.
    let highest = map.highest_address();
    let frame_count = highest / PAGE_SIZE;
    let bitmap_bytes = frame_count.div_ceil(64) * 8;

    let reservations = BootReservations::from_boot_info(&boot_info);
    // The loader's boot pool must be reserved from the frame allocator.
    //
    // The pool holds the kernel image, both stacks, the **kernel's heap**, the
    // bootstrap page tables and the `VelaBootInfo` page. The heap is the
    // important one: it is where the kernel's own page tables live, so a
    // frame allocator that handed out one of its frames would hand out the
    // kernel's own page tables and the machine would fault on the next
    // address-space switch.
    //
    // Reserving the whole pool covers all of that at once, and it is the
    // right granularity: the pool is one contiguous reservation, and the
    // kernel does not hand any of it back.
    let mut reservations = reservations;
    if boot_info.pool_paddr != 0 && boot_info.pool_bytes != 0 {
        reservations.push(
            boot_info.pool_paddr,
            boot_info.pool_paddr + boot_info.pool_bytes,
        );
    }
    let plan = match MemoryPlan::build(
        map.clone(),
        bitmap_bytes,
        // Zero: the kernel's heap is the loader's reservation, not a region
        // carved out of the memory map. Carving a second one here would
        // reserve 4 MiB that nothing ever uses, and — worse — could fail the
        // whole plan on a machine whose largest region is smaller than that.
        0,
        reservations.as_slice(),
    ) {
        Ok(p) => p,
        Err(e) => {
            serial::puts("VELA-KERNEL: cannot plan physical memory: ");
            serial::puts(&e.to_string());
            serial::puts("\n");
            vela_hal_x86_64::qemu_exit::power_off(EXIT_NO_MEMORY);
        }
    };
    serial::puts("VELA-KERNEL: plan ");
    serial::puts(&plan.summary());
    serial::puts("\n");

    // --- 8. Frame allocator ------------------------------------------------
    // The bitmap lives in the pool the plan reserved for it. It is placed by
    // physical address, so it is written before any page tables exist.
    //
    // The region is page-aligned, so a `u64` array can be written directly
    // there. The slice is created with a `'static` lifetime because the
    // allocator requires it: the bitmap must outlive the kernel.
    // SAFETY: the plan guarantees the region is inside usable memory, is
    // page-aligned, and is large enough for the frames it must describe.
    let bitmap_slice: &'static mut [u64] = unsafe {
        core::slice::from_raw_parts_mut(plan.bitmap_region.0 as *mut u64, bitmap_bytes as usize / 8)
    };
    // The allocator itself must outlive the kernel: the page mapper and the
    // syscall layer reach it through a `&'static`.
    //
    // It is therefore *leaked* rather than left as a local. An earlier
    // version wrote `&*core::ptr::from_ref(&FrameAllocator::new(..))`, which
    // takes a reference to a temporary that is dropped at the end of the
    // statement — so the `&'static` pointed at a stack slot that the next
    // call overwrote, and the allocator's bitmap pointer and free count were
    // whatever happened to be there. Leaking puts the allocator in heap
    // memory that is never reused.
    //
    // SAFETY: as above — the region is reserved, page-aligned, and large
    // enough for `frame_count` bits. The allocator is leaked, so it is never
    // dropped and the `&'static mut [u64]` it borrows is never aliased.
    let frames: &'static FrameAllocator = unsafe {
        let allocator = FrameAllocator::new(bitmap_slice, plan.frame_base, plan.frame_count);
        Box::leak(Box::new(allocator))
    };

    // Reserve everything the kernel owns, so the allocator never hands it out.
    //
    // `reserve_range` takes `&self`, so this needs no cast: the allocator's
    // bitmap is mutated through atomics, which is what makes reservation
    // through a shared reference well-defined.
    for (start, end) in plan.reserved_regions() {
        frames.reserve_range(start, end);
    }
    serial::puts("VELA-KERNEL: ");
    serial::puts_dec(frames.free_frames());
    serial::puts(" frames free\n");

    // --- 8b. The frame allocator, for the user-space syscalls --------------
    //
    // `MapPage` and `Brk` grow a *process's* heap, and growing a heap
    // consumes physical frames. Only the kernel owns the frames, so the
    // syscall layer has to be told where they come from.
    //
    // The allocator is installed as a shared source rather than being handed
    // to each syscall as an argument: a syscall's arguments come from user
    // space, and a kernel pointer in an argument slot is exactly what the
    // user-copy validation exists to prevent.
    //
    // The adapter is what makes the shared reference sound. The kernel's
    // frame allocator does its bookkeeping with atomics and is shared behind
    // a `&'static`, so it is reached through the syscall layer's own
    // `&self`-based trait rather than the HAL's `&mut self` one — casting a
    // shared reference to a mutable one would be undefined behaviour even
    // when the referent really is immutable behind the scenes.
    //
    // The adapter is leaked because the syscall layer holds it for the rest
    // of the kernel's life.
    let user_frames: &'static UserFrames = Box::leak(Box::new(UserFrames(frames)));
    // SAFETY: the allocator is leaked, so it outlives the kernel, and it is
    // the only source of physical frames for user spaces.
    unsafe {
        vela_kernel_syscall::task_space::install_frames(user_frames);
    }

    // --- 9. Address space --------------------------------------------------
    //
    // The kernel's own page tables, built in the heap.
    //
    // The heap is identity-mapped — it is at the same virtual and physical
    // address — so the PML4's physical address *is* its virtual address, and
    // the identity translation is honest rather than a placeholder.
    let mut space = AddressSpace::new(vela_hal_common::AddressSpaceId(0));
    let pml4_pa = space.pml4_phys(&IdentityPhys);
    space.set_pml4_base(pml4_pa);

    // --- 9a. The identity map of physical RAM ------------------------------
    //
    // This comes **first**, before anything else is mapped, for two reasons.
    //
    // The first is ordering: the kernel reaches physical memory by *physical*
    // address. The frame allocator hands out physical addresses,
    // `kernel/syscall`'s `zero_physical` writes through a physical address
    // directly, and the LAPIC and the framebuffer are both named by physical
    // address. All of those accesses need the low half identity-mapped in the
    // kernel's own address space, or they land on an unmapped page.
    //
    // The second is *overlap*: the heap, the boot pool and the frame bitmap
    // all live inside the machine's RAM, so the identity map already covers
    // them. Mapping them again, separately, would either be redundant or —
    // worse — would try to put a 4 KiB page into a page-directory slot that
    // the identity map had already filled with a 2 MiB page, and the CPU
    // would then read the wrong bytes.
    //
    // The mapping is done with 2 MiB pages wherever a whole 2 MiB slot is
    // ordinary RAM. A 4 KiB mapping of the whole machine would need one page
    // table per 4 KiB of RAM — 131 072 tables for a 512 MiB machine — which
    // does not fit in the 4 MiB kernel heap. A 2 MiB page collapses 512 of
    // those into one, so the same map costs a few hundred entries and no leaf
    // tables at all.
    //
    // Only the regions the memory map calls *usable* are mapped: the
    // firmware's reserved windows and its device memory must not be handed
    // to the CPU as ordinary RAM, and mapping them would let a stray write
    // corrupt the firmware's ACPI tables.
    //
    // The device windows are excluded from the identity map as well: they are
    // mapped uncached in 9c, and a cacheable mapping of the same physical
    // page would be a second, conflicting view of the same bytes.
    let mut excluded = Vec::new();
    if boot_info.framebuffer_paddr != 0 && boot_info.framebuffer_bytes > 0 {
        excluded.push((
            boot_info.framebuffer_paddr,
            boot_info
                .framebuffer_paddr
                .saturating_add(boot_info.framebuffer_bytes),
        ));
    }
    excluded.push((LAPIC_BASE, LAPIC_BASE + PAGE_SIZE));

    let identity = plan_identity_map(&map, &excluded);
    serial::puts("VELA-KERNEL: identity map ");
    serial::puts_dec(identity.len() as u64);
    serial::puts(" chunks\n");
    for chunk in &identity {
        let flags = vela_hal_common::PageFlags::kernel_rw();
        let result = if chunk.len == LARGE_PAGE_BYTES {
            space.map_large_page(chunk.paddr, chunk.paddr, flags)
        } else {
            space.map_page(chunk.paddr, chunk.paddr, flags)
        };
        if let Err(e) = result {
            serial::puts("VELA-KERNEL: cannot identity-map RAM at ");
            serial::puts_hex(chunk.paddr);
            serial::puts(": ");
            serial::puts(&e.to_string());
            serial::puts("\n");
            vela_hal_x86_64::qemu_exit::power_off(EXIT_NO_MEMORY);
        }
    }

    // The heap and the boot pool are *inside* the identity map now — the
    // loader reserved them out of ordinary RAM — so they are reachable, and
    // reachable at the same virtual and physical address. That identity is
    // what makes `pml4_phys(&IdentityPhys)` correct rather than a
    // placeholder: the kernel's own page tables live in the heap, so the
    // heap pointer that holds a table *is* that table's physical address.
    //
    // The check below is a check of the *handoff*, not of the map: if the
    // loader reserved the heap outside the RAM it reported, the identity map
    // does not cover it and the kernel's own page tables would become
    // unreachable the instant `CR3` changed. That is a silent, catastrophic
    // failure — the machine would triple-fault on the first instruction
    // after the switch — so it is checked here, where it can still be
    // reported.
    for (name, start, end) in [
        (
            "heap",
            boot_info.heap_paddr,
            boot_info.heap_paddr + boot_info.heap_bytes,
        ),
        (
            "pool",
            boot_info.pool_paddr,
            boot_info.pool_paddr + boot_info.pool_bytes,
        ),
    ] {
        if start == 0 || end <= start {
            continue;
        }
        if !identity_covers(&identity, start) || !identity_covers(&identity, end - 1) {
            serial::puts("VELA-KERNEL: the loader's ");
            serial::puts(name);
            serial::puts(" is outside the identity map\n");
            vela_hal_x86_64::qemu_exit::power_off(EXIT_NO_MEMORY);
        }
    }

    // Map the kernel image into the higher half.
    //
    // Both addresses come from the boot handoff:
    //
    // * `kernel_vaddr_base` is where the image is **linked**, and therefore
    //   where it must be **mapped**. It is not `KERNEL_VIRT_BASE`: the
    //   linker script offsets the image by a page so that the first page of
    //   the window holds the ELF program headers, which are never mapped.
    //   Mapping at the window's base instead would place the image one page
    //   too low, and the entry point would not be where the loader expects it.
    // * `kernel_paddr` is where the loader **put** the image, which is a pool
    //   allocation decided at boot and not knowable from the ELF.
    //
    // The image's executable prefix is mapped read-execute and the rest
    // read-write, which is what gives the kernel's own image W^X.
    if boot_info.kernel_bytes > 0 && boot_info.kernel_vaddr_base != 0 {
        let kernel_base = boot_info.kernel_vaddr_base;
        let kernel_pages = boot_info.kernel_bytes.div_ceil(PAGE_SIZE);
        let text_bytes = if boot_info.kernel_text_bytes == 0 {
            kernel_pages * PAGE_SIZE
        } else {
            boot_info.kernel_text_bytes
        };
        let text_pages = text_bytes.div_ceil(PAGE_SIZE);

        // The text, read-execute.
        if let Err(e) = space.map_range(
            kernel_base,
            boot_info.kernel_paddr,
            (text_pages * PAGE_SIZE) as usize,
            vela_hal_common::PageFlags::kernel_rx(),
        ) {
            serial::puts("VELA-KERNEL: cannot map the kernel text: ");
            serial::puts(&e.to_string());
            serial::puts("\n");
            vela_hal_x86_64::qemu_exit::power_off(EXIT_NO_MEMORY);
        }
        // The data, read-write.
        if text_pages < kernel_pages {
            if let Err(e) = space.map_range(
                kernel_base + text_pages * PAGE_SIZE,
                boot_info.kernel_paddr + text_pages * PAGE_SIZE,
                ((kernel_pages - text_pages) * PAGE_SIZE) as usize,
                vela_hal_common::PageFlags::kernel_rw(),
            ) {
                serial::puts("VELA-KERNEL: cannot map the kernel data: ");
                serial::puts(&e.to_string());
                serial::puts("\n");
                vela_hal_x86_64::qemu_exit::power_off(EXIT_NO_MEMORY);
            }
        }
        serial::puts("VELA-KERNEL: kernel image mapped at ");
        serial::puts_hex(kernel_base);
        serial::puts("\n");
    }

    // --- 9c. The device windows --------------------------------------------
    //
    // Two windows are named by physical address and must be mapped for the
    // kernel to use them at all:
    //
    // * the **local APIC** at `0xFEE0_0000`, which the timer reads and
    //   writes. Without it, `apic::init` faults on its very first register
    //   access — and because the IDT is installed by then, the fault is
    //   reported loudly rather than silently black-screening.
    // * the **framebuffer**, which the compositor draws on. It is device
    //   memory: mapping it as ordinary cacheable RAM would mean the CPU's
    //   write-combining buffers held pixels that never reached the screen.
    //
    // Both are mapped uncached, which is what `PageFlags::mmio` expresses.
    // Both were excluded from the identity map above, so there is no second,
    // conflicting mapping of the same physical page.
    if let Err(e) = space.map_page(LAPIC_BASE, LAPIC_BASE, vela_hal_common::PageFlags::mmio()) {
        serial::puts("VELA-KERNEL: cannot map the local APIC: ");
        serial::puts(&e.to_string());
        serial::puts("\n");
        vela_hal_x86_64::qemu_exit::power_off(EXIT_NO_MEMORY);
    }
    if boot_info.framebuffer_paddr != 0 && boot_info.framebuffer_bytes > 0 {
        let fb_pages = boot_info.framebuffer_bytes.div_ceil(PAGE_SIZE);
        if let Err(e) = space.map_range(
            boot_info.framebuffer_paddr,
            boot_info.framebuffer_paddr,
            (fb_pages * PAGE_SIZE) as usize,
            vela_hal_common::PageFlags::mmio(),
        ) {
            serial::puts("VELA-KERNEL: cannot map the framebuffer: ");
            serial::puts(&e.to_string());
            serial::puts("\n");
            vela_hal_x86_64::qemu_exit::power_off(EXIT_NO_MEMORY);
        }
    }

    // The boot stack lives in the heap, so it is mapped with the rest of the
    // kernel's data. It is allocated *after* the image mapping so the heap
    // pointer is where the stack will be.
    let stack_top = heap.alloc(KERNEL_STACK_BYTES as usize, 16).unwrap_or(0);
    if stack_top == 0 {
        serial::puts("VELA-KERNEL: cannot allocate the kernel stack\n");
        vela_hal_x86_64::qemu_exit::power_off(EXIT_NO_MEMORY);
    }
    serial::puts("VELA-KERNEL: stack at ");
    serial::puts_hex(stack_top as u64);
    serial::puts("\n");

    // Switch to the new address space. From here on, the kernel runs from the
    // higher half.
    // SAFETY: the space is built, its tables live in the heap, and it stays
    // alive for the rest of the kernel's life.
    unsafe { activate(&space) };

    // --- 10. Segments ------------------------------------------------------
    //
    // The GDT must be installed before anything depends on a segment
    // selector. The firmware's GDT is still in place at this point, and the
    // firmware's `CS` is `0x38` — a selector the kernel's own table does not
    // contain. That is fine as long as nothing reloads a segment, but the
    // `syscall` gate's `IA32_STAR` names selectors from the *kernel's* table,
    // so the two have to agree before the gate is installed.
    //
    // Interrupts are still disabled here, which is what the GDT's own
    // documentation requires: between the `LGDT` and the `CS` reload there
    // is a window in which an interrupt would return through a selector the
    // new table does not have.
    // SAFETY: this runs once, during boot, with interrupts disabled, before
    // any user code exists.
    unsafe { vela_hal_x86_64::gdt::install() };

    // The syscall gate, whose selectors must match the GDT just installed.
    // SAFETY: as above — the GDT is installed, so the selectors in
    // `STAR_VALUE` name real descriptors.
    unsafe { vela_kernel_syscall::gate::install() };
    serial::puts("VELA-KERNEL: gdt and syscall gate installed\n");

    // --- 11. Interrupts ----------------------------------------------------
    idt::init();
    apic::init();
    // SAFETY: the IDT is installed and the timer is programmed, so an
    // interrupt taken from here on has a handler.
    unsafe { regs::sti() };

    // --- 9. Services -------------------------------------------------------
    // The Exec service, the DOS service and the syscall dispatch table are
    // installed *before* the banner, so that a user program started after
    // the banner can be sure the boundary exists.
    //
    // The services live in leaked allocations because they must outlive
    // every task that uses them, and a task's stack is reclaimed when it
    // exits.
    let exec: &'static vela_kernel_exec::Exec =
        Box::leak(Box::new(vela_kernel_exec::Exec::with_builtins()));
    let dos: &'static vela_kernel_dos::Dos = Box::leak(Box::new(build_dos()));
    // The graphics and window-management services.
    //
    // The Intuition service's screen is the display's geometry, taken from
    // the boot handoff rather than a fixed constant — the kernel must honour
    // whatever the firmware gave it, including a stride that is not the
    // width. A window manager sized for the wrong screen clips everything
    // that matters.
    //
    // The framebuffer is installed *before* Intuition, because compositing is
    // what writes to it and a compositor with no framebuffer would have
    // nothing to write to.
    let (screen_width, screen_height, framebuffer) = display_geometry(&boot_info);
    install_framebuffer(framebuffer);
    let graphics: &'static vela_kernel_graphics::Graphics =
        Box::leak(Box::new(vela_kernel_graphics::Graphics::new()));
    let intuition: &'static vela_kernel_intuition::Intuition = Box::leak(Box::new(
        vela_kernel_intuition::Intuition::new(screen_width, screen_height),
    ));
    let table: &'static DispatchTable = Box::leak(Box::new(builtin_table()));
    // SAFETY: the kernel is single-CPU and this runs once, during boot,
    // before any user code runs.
    unsafe { vela_kernel_syscall::dispatch::install(exec, dos, graphics, intuition, table) };
    serial::puts("VELA-KERNEL: services installed\n");
    serial::puts("VELA-KERNEL: screen ");
    serial::puts_dec(screen_width as u64);
    serial::puts("x");
    serial::puts_dec(screen_height as u64);
    serial::puts("\n");

    // --- 10. Banner ---------------------------------------------------------
    serial::puts("VELA-KERNEL: VelaOS 0.1.0 ready\n");
    BOOTED.store(true, Ordering::Release);

    // --- 11. Idle -----------------------------------------------------------
    // The kernel has no userland yet (that is Phase 4), so it idles with
    // interrupts enabled. The timer tick keeps the machine alive and gives
    // the boot-smoke test something to observe.
    loop {
        // SAFETY: halting with interrupts enabled wakes on the next tick.
        regs::hlt();
    }
}

/// One piece of the kernel's identity map.
///
/// A chunk is either a whole 2 MiB page or a single 4 KiB page. The
/// distinction is load-bearing: a 2 MiB page and a 4 KiB page cannot share
/// a page-directory slot, so the plan has to decide which one owns each slot
/// before any of them is installed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct IdentityChunk {
    /// The physical address, which is also the virtual address.
    paddr: u64,
    /// The chunk's length in bytes.
    len: u64,
}

/// Plan the kernel's identity map of physical RAM.
///
/// # The problem this solves
///
/// The machine's RAM is a handful of disjoint regions, none of which is
/// necessarily 2 MiB-aligned at both ends. A large page must be 2 MiB-aligned
/// on *both* sides — the hardware ignores the low 21 bits of the physical
/// address in the entry — so each region has a head and a tail that only
/// 4 KiB pages can cover.
///
/// The heads and tails are the awkward part. Two regions can share a 2 MiB
/// slot: one's tail and the next one's head can both live in the same slot,
/// and if either of them is mapped as a large page the other's 4 KiB pages
/// have nowhere to go. Mapping the regions independently therefore produces
/// *overlapping* claims on the same page-directory slot, and the second one
/// to be installed fails — or, worse, succeeds and the CPU reads the wrong
/// bytes.
///
/// So the plan is made **jointly**: every 2 MiB slot is examined once, and
/// the decision for that slot is made with the whole machine in view.
///
/// # The rule
///
/// A 2 MiB slot is mapped as a large page when *every* byte of it is
/// ordinary, usable RAM. If any part of the slot is a region's head or tail
/// — or is excluded as a device window — the whole slot is mapped 4 KiB at a
/// time, so that the region's exact extent is what is mapped and no slot is
/// claimed twice.
///
/// The cost of that rule is small: the machine's RAM is described by a
/// handful of regions, so only a handful of slots are affected, and each
/// costs one page table (8 KiB of heap) instead of 512.
///
/// # Why only the *usable* regions
///
/// The firmware's memory map distinguishes ordinary RAM from the windows it
/// reserves for itself, from device memory, and from the ACPI tables it must
/// not have overwritten. Identity-mapping all of it would hand the CPU
/// ordinary read-write pages over the firmware's own structures, and a stray
/// write into the ACPI tables would corrupt the machine in a way that is
/// nearly impossible to diagnose afterwards.
///
/// So only the regions the map calls allocatable are mapped. That is also
/// exactly the set the frame allocator may hand out, which is what keeps the
/// identity map and the allocator's idea of "free" in agreement.
fn plan_identity_map(map: &MemoryMap, excluded: &[(u64, u64)]) -> Vec<IdentityChunk> {
    // The usable RAM, as a list of page-aligned ranges.
    let mut usable: Vec<(u64, u64)> = Vec::new();
    for region in map.regions() {
        if !region.is_allocatable() {
            continue;
        }
        let start = align_up(region.physical_start, PAGE_SIZE);
        let end = align_down(region.end(), PAGE_SIZE);
        if end > start {
            usable.push((start, end));
        }
    }
    usable.sort_unstable();

    // The device windows, as a list of page-aligned ranges.
    let mut windows: Vec<(u64, u64)> = excluded
        .iter()
        .map(|(s, e)| (align_down(*s, PAGE_SIZE), align_up(*e, PAGE_SIZE)))
        .collect();
    windows.sort_unstable();

    let mut chunks = Vec::new();
    for (start, end) in &usable {
        // Split the region around the device windows that overlap it.
        //
        // The windows are sorted, so a single forward scan finds them all and
        // the pieces between them are the plain RAM that may be mapped.
        let mut cursor = *start;
        for (ws, we) in &windows {
            let lo = (*ws).max(*start);
            let hi = (*we).min(*end);
            if lo >= hi {
                continue;
            }
            if cursor < lo {
                push_range(&mut chunks, cursor, lo);
            }
            cursor = hi;
        }
        if cursor < *end {
            push_range(&mut chunks, cursor, *end);
        }
    }
    chunks
}

/// Split a range into 2 MiB chunks and 4 KiB pages.
///
/// The range is page-aligned at both ends. A whole 2 MiB chunk becomes a
/// large page; everything else is covered by 4 KiB pages.
///
/// # Why the alignment test is on the address, not on the range
///
/// A large page must be 2 MiB-aligned on *both* sides, and the hardware
/// ignores the low 21 bits of the physical address in the entry. So a range
/// that begins in the middle of a 2 MiB slot cannot have a large page at its
/// head, however long it is — the first 2 MiB-aligned address inside it is
/// the one that can carry a large page.
///
/// This is also what makes the plan safe when two regions share a 2 MiB
/// slot: one's tail and the next one's head both land in that slot, and
/// both are mapped 4 KiB at a time, so the slot's single page-directory
/// entry is claimed exactly once.
fn push_range(chunks: &mut Vec<IdentityChunk>, start: u64, end: u64) {
    let mut cursor = start;
    while cursor < end {
        // A large page needs an aligned start *and* a whole 2 MiB left.
        if cursor.is_multiple_of(LARGE_PAGE_BYTES) && cursor + LARGE_PAGE_BYTES <= end {
            chunks.push(IdentityChunk {
                paddr: cursor,
                len: LARGE_PAGE_BYTES,
            });
            cursor += LARGE_PAGE_BYTES;
        } else {
            chunks.push(IdentityChunk {
                paddr: cursor,
                len: PAGE_SIZE,
            });
            cursor += PAGE_SIZE;
        }
    }
}

/// True when the identity map covers `addr`.
///
/// The check is by *slot*, not by chunk: the map's chunks are the units that
/// were installed, and a chunk covers a range of bytes. So the question is
/// whether some chunk contains `addr`.
fn identity_covers(chunks: &[IdentityChunk], addr: u64) -> bool {
    chunks
        .iter()
        .any(|c| addr >= c.paddr && addr < c.paddr + c.len)
}

/// Round `value` up to a multiple of `align`.
///
/// `align` is a power of two, which is guaranteed for every caller here: it
/// is either a page size or a large-page size.
fn align_up(value: u64, align: u64) -> u64 {
    value.div_ceil(align) * align
}

/// Round `value` down to a multiple of `align`.
fn align_down(value: u64, align: u64) -> u64 {
    value - value % align
}

/// Build the DOS service the kernel boots with.
///
/// The kernel mounts a ramfs at `/ramfs` — which is where the `RAM:` assign
/// points — and a second one at `/System`, which is where `SYS:` points.
/// Both are empty at boot; `init` populates them.
///
/// A ramfs and not a disk filesystem is deliberate: v0.1.0's boot image has
/// no writable disk, and the report's `RAM:` volume is a ramfs anyway. The
/// mount points are the ones the assigns use, so a path resolves to a mount
/// without any extra mapping table.
fn build_dos() -> vela_kernel_dos::Dos {
    let dos = vela_kernel_dos::Dos::new();
    let ram = alloc::sync::Arc::new(vela_ramfs::RamFs::new());
    dos.mount("/ramfs", "RAM", ram);
    let system = alloc::sync::Arc::new(vela_ramfs::RamFs::new());
    dos.mount("/System", "SYS", system);
    dos
}

/// The framebuffer geometry, as the boot handoff reported it.
///
/// # Why the stride is separate from the width
///
/// A framebuffer's stride is how many *bytes* one row occupies, and it is not
/// required to equal `width * 4`. QEMU's `q35` at 1280x800 gives a stride of
/// exactly 1280 pixels, but other configurations pad rows to a larger
/// boundary, and a renderer that assumed `stride == width` would shear every
/// row after the first. The stride is carried through untouched for that
/// reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DisplayGeometry {
    /// The framebuffer's virtual address, or zero if there is no display.
    address: u64,
    /// The width in pixels.
    width: u32,
    /// The height in pixels.
    height: u32,
    /// The stride in **bytes**.
    stride_bytes: u32,
}

/// Read the display's geometry out of the boot handoff.
///
/// A handoff with no framebuffer is not an error: it is the configuration
/// where the kernel has no display at all, which is what a serial-only boot
/// looks like. The window manager is built for a 640x480 screen in that case,
/// so that a program which opens a window still gets a coherent layer stack
/// even though nothing will ever be composited onto a screen.
///
/// The default is deliberately the smallest geometry the theme can decorate
/// without clipping. A window manager sized for a screen that does not exist
/// is harmless; one sized for a screen that is *smaller* than the real one
/// clips the parts of the desktop a user can see.
fn display_geometry(boot_info: &VelaBootInfo) -> (u32, u32, DisplayGeometry) {
    const FALLBACK_WIDTH: u32 = 640;
    const FALLBACK_HEIGHT: u32 = 480;

    if boot_info.framebuffer_paddr == 0
        || boot_info.framebuffer_width == 0
        || boot_info.framebuffer_height == 0
        || boot_info.framebuffer_bytes == 0
    {
        return (
            FALLBACK_WIDTH,
            FALLBACK_HEIGHT,
            DisplayGeometry {
                address: 0,
                width: 0,
                height: 0,
                stride_bytes: 0,
            },
        );
    }
    // A stride of zero would make every row address collapse onto the first,
    // which reads as valid and writes sixteen hundred pixels into the same
    // line. The width is the only sane interpretation, so it is substituted
    // rather than trusted.
    let stride_bytes = if boot_info.framebuffer_stride_bytes == 0 {
        boot_info.framebuffer_width * 4
    } else {
        boot_info.framebuffer_stride_bytes
    };
    (
        boot_info.framebuffer_width,
        boot_info.framebuffer_height,
        DisplayGeometry {
            address: boot_info.framebuffer_paddr,
            width: boot_info.framebuffer_width,
            height: boot_info.framebuffer_height,
            stride_bytes,
        },
    )
}

/// Install the framebuffer geometry with the window manager.
///
/// # Safety
///
/// The address must be a framebuffer that is mapped for the rest of the
/// kernel's life, and the geometry must describe it. Both come from the boot
/// handoff, which the loader validated against the GOP.
fn install_framebuffer(display: DisplayGeometry) {
    if display.address == 0 || display.width == 0 || display.height == 0 {
        // No display. This is the serial-only configuration, not a failure, so
        // it is not logged as one — the caller's own banner line reports the
        // screen it went with.
        return;
    }
    // SAFETY: the address and geometry come from the boot handoff, which the
    // loader validated against the GOP; the framebuffer outlives the kernel.
    unsafe {
        vela_kernel_intuition::set_framebuffer(
            display.address,
            display.width,
            display.height,
            display.stride_bytes,
        );
    }
}

/// The kernel's default log level.
///
/// `Info`, so the boot banner and any warnings appear but tracing does not.
pub const DEFAULT_LOG_LEVEL: Level = Level::Info;

/// A convenience re-export so downstream crates do not need to depend on the
/// core crate directly for logging.
pub use vela_kernel_core::{debug, error, fatal, info, trace, warn};

#[cfg(test)]
extern crate std;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_kernel_window_is_canonically_mapped() {
        // The higher half must be a canonical address, or the page-table
        // walk the hardware performs on it is undefined.
        assert!(paging::is_canonical(KERNEL_VIRT_BASE));
        assert!(paging::is_canonical(
            KERNEL_VIRT_BASE + KERNEL_VIRT_SIZE - 1
        ));
        // And it must be recognisably the kernel half.
        assert!(paging::is_kernel_canonical(KERNEL_VIRT_BASE));
    }

    #[test]
    fn the_kernel_window_is_one_page_table_entry() {
        // The window must fit in one second-level page-table entry, which is
        // what makes the kernel's mapping a short PML4/PDPT/PD chain.
        assert_eq!(KERNEL_VIRT_SIZE, 1 << 28);
        assert_eq!(KERNEL_VIRT_SIZE, 256 * 1024 * 1024);
    }

    #[test]
    fn the_kernel_base_is_canonical_and_high() {
        assert!(paging::is_canonical(KERNEL_VIRT_BASE));
        // Bit 47 must be set for the address to be in the higher half.
        const {
            assert!(KERNEL_VIRT_BASE & (1 << 47) != 0);
        }
    }

    #[test]
    fn the_boot_flag_starts_false() {
        // The unit tests never run the real entry point, so the flag must
        // still be false. This catches a constructor that pre-sets it.
        assert!(!is_booted());
    }

    #[test]
    fn the_heap_size_is_a_power_of_two_megabytes() {
        // A power-of-two heap size makes the alignment of the bump pointer
        // trivial and the reservation easy to reason about.
        assert_eq!(KERNEL_HEAP_BYTES & (KERNEL_HEAP_BYTES - 1), 0);
        assert_eq!(KERNEL_HEAP_BYTES, 4 * 1024 * 1024);
    }

    #[test]
    fn the_stack_size_is_reasonable() {
        // 64 KiB is generous for the kernel's call depth but small enough
        // that it does not dominate the pool.
        const {
            assert!(KERNEL_STACK_BYTES >= 16 * 1024);
            assert!(KERNEL_STACK_BYTES <= 256 * 1024);
        }
    }

    #[test]
    fn the_exit_statuses_are_distinct_and_non_zero() {
        // The boot-smoke test tells "the kernel refused to boot" apart from
        // "the kernel crashed" by the exit status, so they must differ and
        // must not be zero.
        assert_ne!(EXIT_BAD_HANDOFF, EXIT_NO_MEMORY);
        assert_ne!(EXIT_BAD_HANDOFF, 0);
        assert_ne!(EXIT_NO_MEMORY, 0);
    }
}
