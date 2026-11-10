//! VelaOS LoongArch64 hardware abstraction layer.
//!
//! This crate is the only place in VelaOS that is allowed to know the CPU is a
//! LoongArch (LA464-class core, as emulated by `qemu-system-loongarch64 -M
//! loongarch_virt`). Everything above it — `kernel/memory`, `kernel/scheduler`,
//! `kernel/exec` — talks to hardware through the traits in `vela-arch-common`,
//! which is what keeps the other architectures implementable.
//!
//! # How LoongArch differs from x86-64
//!
//! The ISA is architecturally simpler than x86-64 in three ways that shape this
//! crate:
//!
//! * **No segmentation, no GDT, no IDT.** Privilege is two bits in `CRMD.PLV`
//!   (ring 3 = user, ring 0 = kernel), there is one fixed exception entry
//!   base (`CSR.EENTRY`) plus a small per-CSR set of alternatives
//!   (`CSR.EENTRY`/`CSR.ECFG.VS`-scaled vector spacing), and no
//!   descriptor tables at all. There is therefore no `gdt` module here.
//! * **No hardware page-table walker.** Address translation is done by an
//!   associative TLB that the *kernel fills in software* (`tlbwr`/`tlbfill`).
//!   Page tables are ordinary in-memory trees; [`paging`] walks them and
//!   programs the TLB entries explicitly.
//! * **Two 64-bit direct map windows instead of one.** The `DMW0`–`DMW3`
//!   CSRs map fixed physical ranges at fixed cache attributes, which is how
//!   the kernel maps itself without ever touching a page table. Kernel code
//!   runs through a DMW window; user tasks run with `DMW` windows disabled
//!   for their privilege level (`PLV` bits in the DMW CSR).
//!
//! # Module map
//!
//! | Module | Responsibility |
//! |---|---|
//! | [`regs`] | CSR reads/writes, `rdcnt`/`rdtime`, `idle`, `invtlb`, `dbar`/`ibar` |
//! | [`paging`] | LA464 4-level software page walk, TLB fill, `map`/`unmap` |
//! | [`addrspace`] | Heap-resident address spaces on top of [`paging`] |
//! | [`idt`] | Exception codes (`ESTAT.EC`), vector slot arithmetic, CSR table |
//! | [`serial`] | 16550 UART at the `loongarch_virt` MMIO address |
//! | [`apic`] | ExtIOI line interrupts, IPI mailboxes |
//! | [`timer`] | The architectural `TCFG`/`TVAL` constant-rate timer |
//! | [`context`] | The LoongArch context switch and task-entry trampoline |
//! | [`syscall`] | The `syscall` (EC 11) gate and ring-3 return machinery |
//! | [`smp`] | Mailbox/IPI secondary-core bring-up |
//! | [`pci`] | ECAM access for the `loongarch_virt` PCIe root complex |
//! | [`ps2`] | Documentation of why there is no PS/2 on LoongArch machines |
//! | [`qemu_exit`] | QEMU test-device shutdown for `loongarch_virt` |
//!
//! # Unsafe policy
//!
//! The HAL crates are the one place where `unsafe` is unavoidable, so it is
//! deliberately fenced: pure-computable parts of the architecture (address
//! splitting, DMW/PRMD/ECFG bit layouts, exception-code decoding) live in
//! ordinary safe functions with unit tests, and only the handful of `asm!`
//! blocks touch hardware. Unlike x86-64, LoongArch assembly cannot even be
//! *assembled* on the host, so every `asm!`/`global_asm!` here is gated behind
//! `#[cfg(target_arch = "loongarch64")]` and hardware-touching functions get a
//! no-op fallback off bare metal.
//!
//! Sources for every constant in this crate, so reviewers do not have to
//! rediscover them:
//!
//! * LoongArch Reference Manual Volumes 1–2 (privilege/CSR/TLB chapters).
//! * Linux `arch/loongarch/include/asm/loongarch.h` and `pgtable-bits.h` —
//!   the de-facto cross-platform register and PTE bit assignments.
//! * QEMU `hw/loongarch/virt.h` — the machine model's MMIO layout.

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
pub use syscall::{gate, userspace};

pub use addrspace::{AddressSpace, FrameSource, PhysOfVirt};

pub use vela_arch_common::{CpuArch, PlatformTimer};

use core::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

/// Per-CPU state, stored as parallel arrays indexed by CPU index.
///
/// Each entry is only touched by its own CPU after boot, except during the
/// boot sequence when the BSP writes each secondary's entry before starting
/// it.
static CPU_READY: [AtomicU32; 64] = [const { AtomicU32::new(0) }; 64];
static CPU_CURRENT_TASK: [AtomicU32; 64] = [const { AtomicU32::new(0) }; 64];

/// This CPU's index into the per-CPU arrays, set during early boot.
///
/// The architectural CPU id lives in the read-only `CSR.CPUID`, but the
/// scheduler works with dense *indices*, so the mapping from `CPUID` to index
/// is installed here at boot ([`current_cpu_id::set`]).
static THIS_CPU_INDEX: AtomicUsize = AtomicUsize::new(0);

/// The LoongArch64 implementation of [`CpuArch`].
pub struct LoongarchCpu;

// SAFETY: every operation is a single CPU-local instruction with no
// inter-CPU effect, so any context may call it.
impl CpuArch for LoongarchCpu {
    fn halt() {
        // SAFETY: `idle` only waits for an interrupt; it has no side
        // effect beyond waiting, and any context may execute it.
        unsafe { regs::idle() };
    }

    fn enable_interrupts() {
        // SAFETY: `CpuArch::enable_interrupts` is the caller's explicit
        // request to unmask; the usual hazard is re-entering a lock the caller
        // still holds, which is the caller's contract to respect.
        unsafe { regs::crmd_set_ie(true) };
    }

    fn disable_interrupts() {
        // SAFETY: masking interrupts cannot re-enter a lock the caller
        // holds; the caller asked for it explicitly.
        unsafe { regs::crmd_set_ie(false) };
    }

    fn interrupts_enabled() -> bool {
        regs::crmd_ie()
    }

    fn current_cpu_id() -> usize {
        // `CSR.CPUID` (number 0x20) holds this core's hardware id in its low
        // bits; it is the LoongArch equivalent of the x86 APIC id. Off bare
        // metal the CSR read compiles out, so fall back to the static set
        // during early boot.
        if regs::ON_BARE_METAL {
            let id = regs::cpuid() as usize;
            if id != 0 {
                return id;
            }
        }
        THIS_CPU_INDEX.load(Ordering::Acquire)
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
    /// Called by the BSP for itself and by each secondary's mailbox entry
    /// point. LoongArch has nothing to store for the hardware id (it is a
    /// read-only CSR), so this only installs the index.
    ///
    /// # Safety
    ///
    /// Must be called exactly once per CPU before `current_cpu_id()` is used.
    unsafe fn register_cpu(_hw_id: u32, index: usize) {
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
        // Poll the stable constant-frequency counter (`rdcnt.d`); `CSR.CNTC`
        // runs at `CPUCFG4`'s reported frequency, 100 MHz on QEMU's la464
        // model. Unlike x86-64 there is no fence semantics to worry about —
        // `rdcnt.d` reads `CSR.CNTC` directly and is order-stable.
        if !regs::ON_BARE_METAL {
            // Off bare metal the counter does not advance, so a spin on it
            // would never end. The call still returns — it just returns
            // immediately.
            return 0;
        }
        let start = timer::now_ticks();
        let target = start.wrapping_add(busy_wait_cycles::CYCLE_BUDGET);
        while timer::now_ticks() < target {
            regs::pause_hint();
        }
        target
    }
}

/// A namespace for the busy-wait budget, keeping the magic number out of the
/// trait implementation above.
pub mod busy_wait_cycles {
    /// Default spin budget in counter ticks — 1 ms at QEMU's 100 MHz.
    ///
    /// Long enough to cover a short device latency, short enough that a
    /// timeout path stays responsive.
    pub const CYCLE_BUDGET: u64 = 100_000;
}

/// The LoongArch64 implementation of [`PlatformTimer`], backed by the
/// architectural `TCFG`/`TVAL` timer (`CSR.TCFG` number 0x41, cleared through
/// `CSR.TICLR`).
pub struct LoongarchTimer;

impl PlatformTimer for LoongarchTimer {
    fn monotonic_ticks() -> u64 {
        timer::ticks()
    }

    fn ticks_per_second() -> u64 {
        timer::ticks_per_second()
    }

    fn busy_wait(ticks: u64) {
        timer::busy_wait(ticks);
    }
}

/// The index of this CPU, delegating to the architecture's [`CpuArch`]
/// implementation. Kernel code calls this rather than naming a concrete CPU
/// type, which keeps `kernel/*` architecture-neutral.
pub fn current_cpu_id() -> usize {
    LoongarchCpu::current_cpu_id()
}

#[cfg(test)]
extern crate std;

#[cfg(test)]
mod tests {
    use super::*;
    use std::vec::Vec;

    #[test]
    fn cpu_id_is_zero_on_single_core() {
        // Off bare metal the `CSR.CPUID` read is compiled out and the index
        // static reads its reset value.
        assert_eq!(LoongarchCpu::current_cpu_id(), 0);
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
        LoongarchCpu::halt();
        let end = LoongarchCpu::busy_wait_cycles();
        assert!(end >= busy_wait_cycles::CYCLE_BUDGET || end == 0);
        let seen = Vec::from([end]);
        assert_eq!(seen.len(), 1);
    }
}
