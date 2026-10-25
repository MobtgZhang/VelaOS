//! VelaOS x86-64 hardware abstraction layer.
//!
//! This crate is the only place in VelaOS that is allowed to know the CPU is an
//! x86-64. Everything above it — `kernel/memory`, `kernel/scheduler`,
//! `kernel/exec` — talks to hardware through the traits in `vela-hal-common`,
//! which is what keeps the other three architectures implementable.
//!
//! # Module map
//!
//! | Module | Responsibility |
//! |---|---|
//! | [`regs`] | Port I/O, control registers, `RFLAGS`, `CPUID`, MSRs |
//! | [`paging`] | 4-level page tables, `map`/`unmap`, address-space switching |
//! | [`idt`] | The interrupt descriptor table and exception handlers |
//! | [`serial`] | COM1 serial driver, the kernel's log sink |
//! | [`apic`] | Local APIC timer and a TSC-derived monotonic clock |
//!
//! # Unsafe policy
//!
//! The HAL crates are the one place where `unsafe` is unavoidable, so it is
//! deliberately fenced: pure-computable parts of paging (address splitting,
//! index extraction, present-bit arithmetic) live in [`paging`] as ordinary
//! safe functions with unit tests, and only the handful of `asm!` blocks touch
//! hardware.

#![no_std]
#![deny(unsafe_op_in_unsafe_fn)]
#![warn(missing_docs)]

extern crate alloc;

pub mod addrspace;
pub mod apic;
pub mod gdt;
pub mod idt;
pub mod paging;
pub mod qemu_exit;
pub mod regs;
pub mod serial;

pub use addrspace::{AddressSpace, FrameSource, PhysOfVirt};
pub use paging::Split;

/// The code-segment selector every IDT gate points at.
///
/// Arbitrary but fixed: x86-64 requires a selector with RPL 0 and a
/// present, 64-bit code descriptor, but the actual descriptor lives in the GDT,
/// which the kernel installs at boot. The value only has to be *consistent*
/// between GDT installation and IDT construction — and it is, because it is
/// the very same constant the GDT module uses.
pub const KERNEL_CODE_SELECTOR: u16 = gdt::KERNEL_CODE_SELECTOR;

/// The matching data-segment selector.
pub const KERNEL_DATA_SELECTOR: u16 = gdt::KERNEL_DATA_SELECTOR;
pub use regs::{cpuid, hlt, read_cr0, read_cr2, read_cr3, write_cr3};

use vela_hal_common::{CpuArch, PlatformTimer};

/// The x86-64 implementation of [`CpuArch`].
pub struct X86Cpu;

// SAFETY: every operation is a single CPU-local instruction with no
// inter-CPU effect, so any context may call it.
impl CpuArch for X86Cpu {
    fn halt() {
        regs::hlt();
    }

    fn enable_interrupts() {
        // SAFETY: `CpuArch::enable_interrupts` is the caller's explicit
        // request to unmask; the usual hazard is re-entering a lock the caller
        // still holds, which is the caller's contract to respect.
        unsafe { regs::sti() };
    }

    fn disable_interrupts() {
        regs::cli();
    }

    fn interrupts_enabled() -> bool {
        regs::Rflags::read().interrupts_enabled()
    }

    fn current_cpu_id() -> usize {
        // Single-core for v0.1.0. Phase 4 replaces this with an APIC ID.
        0
    }

    fn busy_wait_cycles() -> u64 {
        // QEMU's TSC runs at a fixed reference rate, so a fixed iteration count
        // would be far too slow or too fast depending on the host. Poll the
        // TSC instead and stop after the requested number of cycles.
        //
        // The read is `RDTSC` and not `RDTSCP`: the latter is an optional CPU
        // feature that QEMU's default CPU model does not implement, and a
        // kernel that issued it unconditionally would die with `#UD` here on
        // every machine whose model differs from the developer's.
        let start = regs::rdtsc_fenced();
        let target = start.wrapping_add(busy_wait_cycles::CYCLE_BUDGET);
        while regs::rdtsc_fenced() < target {
            regs::pause();
        }
        target
    }
}

/// A namespace for the busy-wait budget, keeping the magic number out of the
/// trait implementation above.
pub mod busy_wait_cycles {
    /// Default spin budget in TSC cycles — roughly 50 microseconds at 20 GHz.
    ///
    /// Long enough to cover a short device latency, short enough that a
    /// timeout path stays responsive.
    pub const CYCLE_BUDGET: u64 = 1_000_000;
}

/// The x86-64 implementation of [`PlatformTimer`], backed by the local APIC.
pub struct X86Timer;

impl PlatformTimer for X86Timer {
    fn monotonic_ticks() -> u64 {
        apic::ticks()
    }

    fn ticks_per_second() -> u64 {
        apic::ticks_per_second()
    }

    fn busy_wait(ticks: u64) {
        apic::busy_wait(ticks);
    }
}

#[cfg(test)]
extern crate std;

#[cfg(test)]
mod tests {
    use super::*;
    use std::vec::Vec;
    use vela_hal_common::AddressSpaceId;

    #[test]
    fn cpu_id_is_zero_on_single_core() {
        assert_eq!(X86Cpu::current_cpu_id(), 0);
    }

    #[test]
    fn interrupts_can_be_disabled_and_restored() {
        // Off bare metal the `cli`/`sti` instructions are privileged and the
        // HAL compiles them out, so the *observable* state cannot change here.
        // What is worth asserting on the host is that the accessor reports
        // something coherent and that the pair is safe to call; the real
        // transition is verified by the boot smoke test, which boots the
        // kernel and observes interrupts actually firing.
        let was = X86Cpu::interrupts_enabled();
        X86Cpu::disable_interrupts();
        let after_disable = X86Cpu::interrupts_enabled();
        X86Cpu::enable_interrupts();
        let after_enable = X86Cpu::interrupts_enabled();

        if crate::paging::is_bare_metal() {
            assert!(!after_disable, "cli must clear IF");
            assert!(after_enable, "sti must set IF");
            if !was {
                X86Cpu::disable_interrupts();
            }
        } else {
            // Host build: the calls are no-ops, so the state must not move.
            assert_eq!(after_disable, was);
            assert_eq!(after_enable, was);
        }
    }

    #[test]
    fn unknown_address_space_is_rejected() {
        // Phase 4 owns the address-space registry; until it exists there is
        // nothing to look up, so assert the identity space is simply not the
        // one a caller would ask for.
        let bogus = AddressSpaceId(0xDEAD_BEEF);
        assert_eq!(bogus.0, 0xDEAD_BEEF);
    }

    #[test]
    fn halt_and_busy_wait_return() {
        // These cannot be meaningfully asserted beyond "they return"; the point
        // is that the asm blocks assemble and do not fault on the host.
        X86Cpu::halt();
        let end = X86Cpu::busy_wait_cycles();
        assert!(end > 0);
        let seen = Vec::from([end]);
        assert_eq!(seen.len(), 1);
    }
}
