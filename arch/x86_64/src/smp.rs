//! Symmetric multiprocessing: bringing up Application Processors.
//!
//! # The boot sequence
//!
//! The BSP runs the normal kernel entry sequence: serial, boot info, paging,
//! GDT, IDT and the local APIC. Once that is complete it wakes each
//! Application Processor with the documented INIT / INIT-level-deassert / SIPI
//! sequence.
//!
//! Each AP starts at a real-mode trampoline copied into low memory. The
//! trampoline switches to protected mode, reads its own APIC ID, selects its
//! per-AP configuration, loads the BSP's PML4 into `CR3`, enables PAE and long
//! mode, and jumps to [`ap_entry`]. The AP then installs its own GDT and IDT,
//! initialises its local APIC timer, registers itself with the HAL and spins
//! in the idle loop.
//!
//! # Why the trampoline lives below 1 MiB
//!
//! A Startup IPI carries only the target's initial page number in its eight-bit
//! vector field. Execution therefore begins at `vector << 12`, which is always
//! below 1 MiB. The trampoline and its configuration table are placed in that
//! window and mapped identity-wise into the kernel's address space before any
//! AP is started; otherwise the AP would fault the instant it loaded the BSP's
//! page tables.
//!
//! # Per-CPU state
//!
//! The HAL's arrays are indexed by CPU index, which every CPU can derive from
//! its local APIC ID via [`crate::CpuArch::current_cpu_id`]. After boot a CPU
//! reads only its own entry, so there is no cross-CPU contention in the arrays.

use core::sync::atomic::{AtomicU32, Ordering};
use spin::Mutex;

use crate::{apic, gdt, idt, regs, serial, CpuArch, X86Cpu};

/// How long the BSP waits for all APs to signal ready, in milliseconds.
const AP_READY_TIMEOUT_MS: u64 = 5000;

/// The trampoline's real-mode load address.
///
/// The vector field of a Startup IPI is the page number, so this address must
/// be page-aligned. `0x7000` gives vector `7`.
pub const TRAMPOLINE_ADDR: u64 = 0x0000_7000;

/// The Startup IPI vector that points at [`TRAMPOLINE_ADDR`].
pub const TRAMPOLINE_VECTOR: u8 = 7;

/// Low pages the trampoline, GDT and per-AP configuration table occupy.
///
/// The table runs from `0x8000` to `0x9FC0`, so three pages are needed.
pub const TRAMPOLINE_PAGES: [u64; 3] = [0x0000_7000, 0x0000_8000, 0x0000_9000];

/// Where the trampoline stores the GDT descriptor pointer (limit + base).
const GDT_POINTER_ADDR: u64 = 0x0000_7080;

/// Where the trampoline stores a copy of the BSP's GDT bytes.
const GDT_COPY_ADDR: u64 = 0x0000_7100;

/// Base of the per-AP configuration table.
///
/// Entry `n` is at `CONFIG_BASE + n * CONFIG_STRIDE`.
const CONFIG_BASE: u64 = 0x0000_8000;

/// Bytes per configuration entry: PML4, AP entry, stack top, APIC ID.
const CONFIG_STRIDE: u64 = 32;

/// Serialises AP table installation.
///
/// The GDT and IDT are currently kernel-wide statics, so two APs installing
/// them at the same instant would race. The lock makes the installation
/// sequential; it is not a scheduler lock and is held only during AP boot.
static AP_INIT_LOCK: Mutex<()> = Mutex::new(());

/// How many APs have signalled ready so far.
static AP_READY_COUNT: AtomicU32 = AtomicU32::new(0);

/// Initialise the SMP subsystem and wake every Application Processor.
///
/// # Safety
///
/// Must be called exactly once, from the BSP, after the GDT, IDT and APIC are
/// installed and interrupts are enabled. The caller must have mapped every page
/// in [`TRAMPOLINE_PAGES`] into the kernel's current address space.
pub unsafe fn bring_up_aps(cpu_count: usize, pml4_pa: u64, stacks: &[u64]) {
    if cpu_count <= 1 {
        return;
    }
    if cpu_count > 256 {
        serial::puts("VELA-KERNEL: SMP: too many processors\n");
        return;
    }
    if stacks.len() < cpu_count - 1 {
        serial::puts("VELA-KERNEL: SMP: no AP stacks were supplied\n");
        return;
    }

    serial::puts("VELA-KERNEL: SMP: waking ");
    serial::puts_dec((cpu_count - 1) as u64);
    serial::puts(" application processors\n");

    copy_trampoline();

    // Write one configuration entry per AP before any AP starts. The AP reads
    // its APIC ID from its local APIC and selects the matching entry.
    for apic_id in 1..cpu_count as u32 {
        write_config(
            apic_id,
            pml4_pa,
            ap_entry_address(),
            stacks[(apic_id - 1) as usize],
        );
    }

    // INIT resets the AP to real mode; the deassert command resynchronises the
    // APIC arbitration; SIPI starts execution at the trampoline. The second
    // SIPI covers the documented case where the first one is missed.
    for apic_id in 1..cpu_count as u32 {
        // SAFETY: the APIC is initialised and the target ID is below the
        // firmware's processor count.
        unsafe { apic::send_init_ipi(apic_id) };
        regs::spin_hint(20_000);
        // SAFETY: as above.
        unsafe { apic::send_init_level_deassert(apic_id) };
        regs::spin_hint(20_000);
        // SAFETY: the trampoline page is mapped and reserved.
        unsafe { apic::send_startup_ipi(apic_id, TRAMPOLINE_VECTOR) };
        regs::spin_hint(300_000);
        // SAFETY: as above.
        unsafe { apic::send_startup_ipi(apic_id, TRAMPOLINE_VECTOR) };
        regs::spin_hint(300_000);
    }

    // Wait for every AP to signal ready. The BSP's timer keeps running, so
    // the deadline is measured in timer ticks rather than wall-clock sleeps.
    let start = apic::ticks();
    let ticks_per_ms = apic::ticks_per_second() / 1000;
    let deadline_ticks = AP_READY_TIMEOUT_MS * ticks_per_ms;

    while AP_READY_COUNT.load(Ordering::Acquire) < (cpu_count - 1) as u32 {
        if apic::ticks().wrapping_sub(start) > deadline_ticks {
            serial::puts("VELA-KERNEL: SMP: timeout waiting for APs\n");
            break;
        }
        regs::pause();
    }

    serial::puts("VELA-KERNEL: SMP: ");
    serial::puts_dec(AP_READY_COUNT.load(Ordering::Acquire) as u64);
    serial::puts(" APs online\n");
}

/// Called by each AP's entry point to signal that it is ready to run tasks.
pub fn ap_ready() {
    AP_READY_COUNT.fetch_add(1, Ordering::Release);
}

/// Copy the trampoline, the GDT descriptor pointer and the GDT bytes to low
/// memory.
fn copy_trampoline() {
    let trampoline = trampoline_bytes();
    // SAFETY: the caller mapped every trampoline page into the kernel's
    // address space before this call.
    unsafe {
        core::ptr::copy_nonoverlapping(
            trampoline.as_ptr(),
            TRAMPOLINE_ADDR as *mut u8,
            trampoline.len(),
        );
    }

    // The trampoline loads the GDT with `lgdt [0x7080]`. The descriptor
    // pointer is a 16-bit limit followed by a 64-bit base.
    unsafe {
        write_u64_at(GDT_POINTER_ADDR + 2, GDT_COPY_ADDR);
        core::ptr::write(GDT_POINTER_ADDR as *mut u16, (gdt::GDT_BYTES - 1) as u16);
    }

    // Copy the BSP's installed table. The AP loads exactly the same selectors
    // that the BSP used, and `ap_entry` later installs its own equivalent.
    unsafe {
        core::ptr::copy_nonoverlapping(
            gdt::raw_bytes().as_ptr(),
            GDT_COPY_ADDR as *mut u8,
            gdt::GDT_BYTES,
        );
    }
}

/// Write one AP's configuration entry.
fn write_config(apic_id: u32, pml4_pa: u64, entry: u64, stack_top: u64) {
    let addr = CONFIG_BASE + apic_id as u64 * CONFIG_STRIDE;
    // SAFETY: `addr` is inside the mapped configuration pages.
    unsafe {
        write_u64_at(addr, pml4_pa);
        write_u64_at(addr + 8, entry);
        write_u64_at(addr + 16, stack_top);
        write_u64_at(addr + 24, apic_id as u64);
    }
}

/// Write a little-endian `u64` at a physical/virtual address.
unsafe fn write_u64_at(addr: u64, value: u64) {
    // SAFETY: the caller guarantees the address is mapped and writable.
    unsafe {
        core::ptr::write_unaligned(addr as *mut u64, value);
    }
}

/// The address of [`ap_entry`].
fn ap_entry_address() -> u64 {
    // Convert the function's code address through a raw pointer. This does not
    // call the function; the trampoline uses the address after building the
    // long-mode environment.
    ap_entry as *const () as usize as u64
}

/// Machine code for the AP trampoline.
///
/// The bytes are assembled by hand because the trampoline must run in 16-bit
/// and 32-bit modes before switching to long mode. The layout is:
///
/// ```text
/// 0x7000: real mode: disable interrupts, set segments and stack,
///         load the GDT, enter protected mode, far-jump to 0x7020.
/// 0x7020: protected mode: read this CPU's APIC ID, select its config entry,
///         load CR3, enable PAE, set the long-mode MSR, enable long mode,
///         far-jump to 0x7072.
/// 0x7072: long mode: load the AP entry, stack top and APIC ID from the
///         config entry; set RSP; jump to the entry point.
/// ```
///
/// No instruction overlaps `0x7080`, where the GDT descriptor pointer lives.
fn trampoline_bytes() -> &'static [u8] {
    &[
        // Real mode.
        0xFA, // cli
        0x31, 0xC0, // xor ax, ax
        0x8E, 0xD8, // mov ds, ax
        0x8E, 0xC0, // mov es, ax
        0x8E, 0xD0, // mov ss, ax
        0xBC, 0x00, 0x7F, // mov sp, 0x7F00
        0x0F, 0x01, 0x16, 0x80, 0x70, // lgdt [0x7080]
        0x0F, 0x20, 0xC0, // mov eax, cr0
        0x66, 0x83, 0xC8, 0x01, // or eax, 1
        0x0F, 0x22, 0xC0, // mov cr0, eax
        0xEA, 0x20, 0x70, 0x08, 0x00, // far jmp 0x08:0x7020
        // Protected mode.
        0x66, 0xB8, 0x10, 0x00, // mov ax, 0x10
        0x8E, 0xD8, // mov ds, ax
        0x8E, 0xC0, // mov es, ax
        0x8E, 0xD0, // mov ss, ax
        0xBC, 0x00, 0x7F, 0x00, 0x00, // mov esp, 0x7F00
        0xBB, 0x00, 0x00, 0xE0, 0xFE, // mov ebx, 0xFEE00000
        0x8B, 0x43, 0x20, // mov eax, [ebx + 0x20] (APIC ID)
        0xC1, 0xE8, 0x18, // shr eax, 24
        0xC1, 0xE0, 0x05, // shl eax, 5
        0x05, 0x00, 0x80, 0x00, 0x00, // add eax, 0x8000
        0x89, 0xC3, // mov ebx, eax
        0x8B, 0x0B, // mov ecx, [ebx] (PML4)
        0x0F, 0x22, 0xD9, // mov cr3, ecx
        0x0F, 0x20, 0xE0, // mov eax, cr4
        0x83, 0xC8, 0x20, // or eax, 0x20 (PAE)
        0x0F, 0x22, 0xE0, // mov cr4, eax
        0xB9, 0x80, 0x00, 0x00, 0xC0, // mov ecx, 0xC0000080
        0x0F, 0x32, // rdmsr
        0x0D, 0x00, 0x01, 0x00, 0x00, // or eax, 0x100 (long mode)
        0x0F, 0x30, // wrmsr
        0x0F, 0x20, 0xC0, // mov eax, cr0
        0x0D, 0x00, 0x00, 0x00, 0x80, // or eax, 0x80000000 (paging)
        0x0F, 0x22, 0xC0, // mov cr0, eax
        0xEA, 0x72, 0x70, 0x00, 0x00, 0x08, 0x00, // far jmp 0x08:0x7072
        // Long mode.
        0x48, 0x8B, 0x43, 0x08, // mov rax, [rbx + 8]
        0x48, 0x8B, 0x63, 0x10, // mov rsp, [rbx + 16]
        0x48, 0x8B, 0x7B, 0x18, // mov rdi, [rbx + 24]
        0xFF, 0xE0, // jmp rax
    ]
}

/// Entry point for each Application Processor.
///
/// Called after the trampoline has switched to long mode.
///
/// # Safety
///
/// Must be called exactly once per AP, after the trampoline has established a
/// 64-bit environment and selected this CPU's configuration. `apic_id` must be
/// a valid APIC ID below the HAL's per-CPU array bound.
pub unsafe extern "C" fn ap_entry(apic_id: u32) -> ! {
    // SAFETY: the trampoline's configuration supplies this CPU's own APIC ID.
    unsafe { X86Cpu::register_cpu(apic_id, apic_id as usize) };

    // Install the kernel-wide GDT and IDT. The lock prevents two APs from
    // writing the shared statics at once; the tables are identical.
    {
        let _guard = AP_INIT_LOCK.lock();
        // SAFETY: this runs during AP boot, with interrupts disabled.
        unsafe { gdt::install() };
        idt::init();
        apic::init();
    }

    // Mark this CPU ready before signalling the BSP.
    X86Cpu::mark_cpu_ready();
    ap_ready();

    serial::puts("VELA-KERNEL: SMP: AP ");
    serial::puts_dec(apic_id as u64);
    serial::puts(" online\n");

    // Spin in the idle loop with interrupts enabled.
    // SAFETY: the IDT is installed and the timer is programmed.
    unsafe { regs::sti() };
    loop {
        regs::hlt();
    }
}
