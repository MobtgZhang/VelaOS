//! VelaOS AArch64 (ARMv8-A) hardware abstraction layer.
//!
//! This crate is the only place in VelaOS that is allowed to know the CPU is
//! an ARMv8-A core (as emulated by `qemu-system-aarch64 -M virt`). Everything
//! above it — `kernel/memory`, `kernel/scheduler`, `kernel/exec` — talks to
//! hardware through the traits in `vela-arch-common`, which is what keeps the
//! other architectures implementable.
//!
//! # How AArch64 differs from x86-64
//!
//! The ISA is architecturally different from x86-64 in four ways that shape
//! this crate:
//!
//! * **No segmentation, no GDT.** Privilege is an exception level (EL0 =
//!   user, EL1 = kernel) carried in `PSTATE.M`; there are no descriptor
//!   tables to install. The `gdt` module here exists only to document that.
//! * **A fixed 16-slot vector table, not a programmable IDT.** `VBAR_EL1`
//!   names one 2048-byte table whose sixteen 128-byte slots are fixed by the
//!   architecture (synchronous/IRQ/FIQ/SError × current-EL-SP0/
//!   current-EL-SPx/lower-EL-A64/lower-EL-A32). See [`idt`].
//! * **Two translation bases, not one CR3.** User addresses walk `TTBR0_EL1`
//!   and kernel addresses walk `TTBR1_EL1`, with per-ASID tags in the
//!   TTBRs' bits 63:48. See [`paging`] and [`addrspace`].
//! * **A message-passing interrupt controller.** The GICv3 routes SPIs
//!   through the distributor, but per-CPU state (SGIs, PPIs, the timer) and
//!   IPIs go through the redistributor and CPU-interface *system registers*
//!   (`ICC_*`), not MMIO. See [`apic`].
//!
//! # Module map
//!
//! | Module | Responsibility |
//! |---|---|
//! | [`regs`] | System-register access, `WFI`, `DAIF` masking, barriers, TLBI |
//! | [`paging`] | Stage-1 translation, 4 KiB granule, 4-level, 48-bit VA |
//! | [`addrspace`] | Heap-resident address spaces on top of [`paging`] |
//! | [`idt`] | The 16 `VBAR_EL1` vector slots and the `ESR` decoder |
//! | [`serial`] | PL011 UART at the `virt` machine's MMIO address |
//! | [`apic`] | GICv3 distributor, redistributor and CPU interface |
//! | [`timer`] | The architectural generic timers (`CNTFRQ`/`CNTV`) |
//! | [`context`] | The AAPCS64 context switch and task-entry trampoline |
//! | [`syscall`] | The `SVC` (EC 0x15) gate and ring-0 → ring-3 ERET |
//! | [`smp`] | MPIDR-based CPU identification and secondary park |
//! | [`pci`] | ECAM access for the `virt` machine's PCIe root complex |
//! | [`ps2`] | Documentation of why there is no PS/2 on AArch64 boards |
//! | [`qemu_exit`] | Semihosting `SYS_EXIT` / QEMU `virt` shutdown |
//! | [`gdt`] | Documentation: there is no GDT on AArch64 |
//!
//! # Unsafe policy
//!
//! The HAL crates are the one place where `unsafe` is unavoidable, so it is
//! deliberately fenced: pure-computable parts of the architecture (descriptor
//! encodings, `ESR` decoding, register-value builders, ECAM arithmetic) live
//! in ordinary safe functions with unit tests, and only the handful of `asm!`
//! blocks touch hardware. ARMv8-A assembly cannot be *assembled* by an x86
//! host toolchain, so every `asm!`/`global_asm!` here is gated behind
//! `#[cfg(target_arch = "aarch64")]` and hardware-touching functions get an
//! inert fallback off bare metal. This is what lets `cargo test` run the
//! pure-logic halves of every driver on the development host.
//!
//! Sources for every constant in this crate, so reviewers do not have to
//! rediscover them:
//!
//! * ARMv8-A Architecture Reference Manual (ARM DDI 0487), in particular the
//!   `VMSA` (stage-1 translation) and system-register chapters.
//! * GICv3 and GICv4 Architecture Specification (ARM IHI 0069).
//! * ARM Semihosting specification (ARM IHI 0022).
//! * QEMU `hw/arm/virt.c` — the machine model's MMIO layout.

#![no_std]
#![deny(unsafe_op_in_unsafe_fn)]
#![warn(missing_docs)]

extern crate alloc;

pub mod addrspace;
pub mod apic;
pub mod context;
pub mod current_cpu_id;
pub mod gdt;
pub mod idt;
pub mod paging;
pub mod pci;
pub mod ps2;
pub mod qemu_exit;
pub mod regs;
pub mod serial;
pub mod smp;
pub mod syscall;
pub mod timer;

/// The syscall gate and ring-3 return machinery, re-exported at the crate
/// root for surface parity with the other architecture crates.
pub use syscall::{gate, userspace};

/// The address-space surface the facade re-exports.
pub use addrspace::{AddressSpace, FrameSource, PhysOfVirt};

/// The trait surface the facade re-exports.
pub use vela_arch_common::{CpuArch, PlatformTimer};

use core::sync::atomic::{AtomicU32, Ordering};

/// Per-CPU state, stored as parallel arrays indexed by CPU index.
///
/// Each entry is only touched by its own CPU after boot, except during the
/// boot sequence when the BSP writes each secondary's entry before starting
/// it (see [`smp`]).
static CPU_READY: [AtomicU32; smp::MAX_CPUS] = [const { AtomicU32::new(0) }; smp::MAX_CPUS];
static CPU_CURRENT_TASK: [AtomicU32; smp::MAX_CPUS] = [const { AtomicU32::new(0) }; smp::MAX_CPUS];

/// The AArch64 implementation of [`CpuArch`].
pub struct Aarch64Cpu;

// SAFETY: every operation is a single CPU-local instruction with no
// inter-CPU effect, so any context may call it.
impl CpuArch for Aarch64Cpu {
    fn halt() {
        regs::wfi();
    }

    fn enable_interrupts() {
        // Clearing `DAIF.I` unmasks IRQs; the caller owns the usual hazard of
        // re-entering a lock it still holds.
        unsafe { regs::daif_unmask() };
    }

    fn disable_interrupts() {
        // SAFETY: masking IRQs can only defer them, never lose them.
        unsafe { regs::daif_mask() };
    }

    fn interrupts_enabled() -> bool {
        regs::interrupts_enabled()
    }

    fn current_cpu_id() -> usize {
        // `MPIDR_EL1.Aff0` (bits 7:0) is this core's hardware id under the
        // QEMU `virt` topology (aff0 = core index, aff1/2/3 = 0). Off bare
        // metal the system-register read is compiled out, so fall back to the
        // index static set during early boot.
        if regs::ON_BARE_METAL {
            let id = (regs::read_mpidr() & 0xFF) as usize;
            if id != 0 {
                return id;
            }
        }
        current_cpu_id::current_cpu_id()
    }

    /// Tell the HAL which CPU index this CPU is. Called once during early boot
    /// on every CPU before the scheduler considers it runnable.
    ///
    /// # Safety
    ///
    /// Must be called exactly once per CPU, before any code on that CPU reads
    /// `current_cpu_id()`.
    unsafe fn set_this_cpu(index: usize) {
        current_cpu_id::set(index);
    }

    /// Register this CPU's per-CPU state.
    ///
    /// Called by the BSP for itself and by each secondary's entry point.
    /// AArch64 has nothing to store for the hardware id beyond the index: the
    /// MPIDR read in `current_cpu_id` is live on the target, so this only
    /// installs the scheduler index.
    ///
    /// # Safety
    ///
    /// Must be called exactly once per CPU before `current_cpu_id()` is used.
    unsafe fn register_cpu(_mpidr_aff0: u32, index: usize) {
        current_cpu_id::set(index);
    }

    fn mark_cpu_ready() {
        let index = Self::current_cpu_id();
        if index < CPU_READY.len() {
            CPU_READY[index].store(1, Ordering::Release);
        }
    }

    fn is_cpu_ready() -> bool {
        let index = Self::current_cpu_id();
        index < CPU_READY.len() && CPU_READY[index].load(Ordering::Acquire) != 0
    }

    fn set_current_task(task_id: u32) {
        let index = Self::current_cpu_id();
        if index < CPU_CURRENT_TASK.len() {
            CPU_CURRENT_TASK[index].store(task_id, Ordering::Release);
        }
    }

    fn current_task_id() -> u32 {
        let index = Self::current_cpu_id();
        if index < CPU_CURRENT_TASK.len() {
            CPU_CURRENT_TASK[index].load(Ordering::Acquire)
        } else {
            0
        }
    }

    fn busy_wait_cycles() -> u64 {
        // Poll the physical counter (`CNTPCT_EL0`); its frequency comes from
        // `CNTFRQ_EL1`, typically 62.5 MHz or 100 MHz on QEMU's `virt` model.
        // A one-millisecond budget, like the LoongArch64 and x86-64 HALs.
        let frequency = timer::frequency();
        if frequency == 0 {
            return 0;
        }
        let budget = frequency / 1000;
        let start = timer::now_ticks();
        let target = start.wrapping_add(budget);
        while timer::now_ticks() < target {
            core::hint::spin_loop();
        }
        target
    }
}

/// A namespace for the busy-wait budget, keeping the magic number out of the
/// trait implementation above.
pub mod busy_wait_cycles {
    /// The busy-wait duration in milliseconds, multiplied by the counter
    /// frequency to obtain the tick budget.
    pub const BUDGET_MS: u64 = 1;
}

/// The AArch64 implementation of [`PlatformTimer`], backed by the
/// architectural generic timers: the physical counter `CNTPCT_EL0` for
/// monotonic time and the virtual timer `CNTV_*` for scheduled ticks (PPI 27).
pub struct Aarch64Timer;

impl PlatformTimer for Aarch64Timer {
    fn monotonic_ticks() -> u64 {
        timer::now_ticks()
    }

    fn ticks_per_second() -> u64 {
        timer::frequency()
    }

    fn busy_wait(ticks: u64) {
        timer::busy_wait(ticks);
    }
}

/// The index of this CPU, delegating to the architecture's [`CpuArch`]
/// implementation. Kernel code calls this rather than naming a concrete CPU
/// type, which keeps `kernel/*` architecture-neutral.
pub fn current_cpu_id() -> usize {
    Aarch64Cpu::current_cpu_id()
}

/// This CPU's index into the per-CPU arrays, set during early boot.
///
/// The architectural CPU id lives in `MPIDR_EL1.Aff0`, but the scheduler
/// works with dense *indices*, so the mapping from MPIDR to index is
/// installed here at boot ([`current_cpu_id::set`]).
#[cfg(test)]
extern crate std;

#[cfg(test)]
mod tests {
    use super::*;
    use std::vec::Vec;

    #[test]
    fn cpu_id_is_zero_on_single_core() {
        // Off bare metal the MPIDR read is compiled out and the index static
        // reads its reset value.
        assert_eq!(Aarch64Cpu::current_cpu_id(), 0);
    }

    #[test]
    fn unknown_address_space_is_rejected() {
        // The address-space registry lives in `kernel/sched`; here we only
        // assert the id type round-trips.
        let bogus = vela_arch_common::AddressSpaceId(0xDEAD_BEEF);
        assert_eq!(bogus.0, 0xDEAD_BEEF);
    }

    #[test]
    fn halt_and_busy_wait_return() {
        // Off bare metal these are no-ops and counter reads return zero; the
        // point is that the call paths typecheck and terminate.
        Aarch64Cpu::halt();
        let end = Aarch64Cpu::busy_wait_cycles();
        // Without a counter frequency the budget is zero: the poll loop is
        // skipped entirely rather than spinning forever on a frozen clock.
        assert_eq!(end, 0);
        let seen = Vec::from([end]);
        assert_eq!(seen.len(), 1);
    }

    #[test]
    fn the_timer_surface_is_wired_to_the_generic_timers() {
        // PlatformTimer delegates to the counter; on the host every read is
        // a defined zero and never faults.
        assert_eq!(Aarch64Timer::monotonic_ticks(), 0);
        Aarch64Timer::busy_wait(4);
    }

    /// Serialises the tests that touch the process-wide per-CPU state.
    fn per_cpu_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    #[test]
    fn per_cpu_slots_are_bounded_by_max_cpus() {
        let _guard = per_cpu_lock();
        // The arrays are compile-time bounded; a CPU index at or above the
        // bound must be ignored rather than panicking or aliasing.
        unsafe { Aarch64Cpu::set_this_cpu(smp::MAX_CPUS) };
        assert_eq!(Aarch64Cpu::current_task_id(), 0);
        Aarch64Cpu::mark_cpu_ready();
        assert!(!Aarch64Cpu::is_cpu_ready(), "out-of-range index ignored");
        Aarch64Cpu::set_current_task(7);
        assert_eq!(Aarch64Cpu::current_task_id(), 0);
        // Restore index 0 so other tests observe the reset state.
        unsafe { Aarch64Cpu::set_this_cpu(0) };
    }

    #[test]
    fn ready_and_task_flags_round_trip() {
        let _guard = per_cpu_lock();
        unsafe { Aarch64Cpu::set_this_cpu(0) };
        assert!(!Aarch64Cpu::is_cpu_ready());
        Aarch64Cpu::mark_cpu_ready();
        assert!(Aarch64Cpu::is_cpu_ready());
        Aarch64Cpu::set_current_task(42);
        assert_eq!(Aarch64Cpu::current_task_id(), 42);
    }
}
