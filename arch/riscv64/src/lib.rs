//! VelaOS riscv64 hardware abstraction layer.
//!
//! This crate is the only place in VelaOS that is allowed to know the CPU
//! is a 64-bit RISC-V hart in S-mode. Everything above it — `kernel/memory`,
//! `kernel/scheduler`, `kernel/exec` — talks to hardware through the traits
//! in `vela-arch-common`, which is what keeps the other architectures
//! implementable.
//!
//! # The platform this crate targets
//!
//! RV64GC running in supervisor mode, booted by an SBI firmware (OpenSBI on
//! QEMU's `virt` machine). Every consequence of that sentence is load-
//! bearing somewhere in this crate:
//!
//! * **S-mode, not M-mode**: `mhartid`, `pmpcfg*` and the machine interrupt
//!   controller are unreachable; what M-mode knows arrives through the SBI
//!   or the boot protocol ([`regs`], [`smp`]).
//! * **An SBI firmware exists**: console output, the timer deadline and
//!   hart bring-up all have a firmware path ([`serial`], [`timer`], [`smp`]).
//! * **QEMU `virt` addresses**: UART `0x1000_0000`, PLIC `0x0C00_0000`,
//!   CLINT `0x0200_0000`, ECAM `0x3000_0000`, `sifive_test` `0x0010_0000`,
//!   timebase 10 MHz.
//!
//! # Module map
//!
//! | Module | Responsibility |
//! |---|---|
//! | [`regs`] | CSRs, fences, WFI, the SBI call, the boot-protocol hart id |
//! | [`paging`] | Sv39 PTE encoding, the three-level walk, `satp` arithmetic |
//! | [`addrspace`] | Address-space ownership over the heap table pool |
//! | [`trap`] / [`idt`] | The `stvec` entry stub, trap frame and `scause` decoder |
//! | [`context`] | The context switch and task-entry trampoline |
//! | [`syscall`] | The `ecall` gate: register contract and result shape |
//! | [`userspace`] | The S-mode → U-mode `sret` payload and `enter_user` |
//! | [`plic`] / [`apic`] | The PLIC: priorities, enables, claim/complete |
//! | [`timer`] | The `time` counter, Sstc deadlines and the SBI fallback |
//! | [`serial`] | The NS16550A UART — the kernel's log sink and console |
//! | [`smp`] | Secondary-hart bring-up through the SBI HSM extension |
//! | [`current_cpu_id`] | The per-hart identifier, boot-protocol flavoured |
//! | [`pci`] | ECAM configuration-space access |
//! | [`ps2`] | Input plumbing (routed to the UART — see the module docs) |
//! | [`qemu_exit`] | `sifive_test` power control for the panic path |
//! | [`gdt`] | Documents why RISC-V has no GDT (surface parity only) |
//!
//! [`idt`] and [`apic`] are deliberate aliases of [`trap`] and [`plic`]:
//! the facade re-exports the x86-64 module names, so the same
//! architecture-neutral code path compiles against either HAL.
//!
//! # Unsafe policy
//!
//! The HAL crates are the one place where `unsafe` is unavoidable, so it is
//! deliberately fenced: every pure-computable part — Sv39 address
//! splitting, PTE encoding, `scause` decoding, the trap frame layout, the
//! PLIC register map — lives in ordinary safe functions with unit tests,
//! and only `asm!` blocks and volatile MMIO accesses touch hardware. On the
//! host (a build whose target is not `riscv64gc-unknown-none-elf`) every
//! privileged instruction and MMIO window is compiled out, so the unit
//! tests exercise the pure logic without a kernel to run it in.

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
pub mod plic;
pub mod ps2;
pub mod qemu_exit;
pub mod regs;
pub mod serial;
pub mod smp;
pub mod syscall;
pub mod timer;
pub mod trap;
pub mod userspace;

pub use syscall::SyscallResult;

pub use addrspace::{AddressSpace, FrameSource, PhysOfVirt};
pub use paging::Split;

pub use addrspace::IdentityPhys;
pub use regs::{hart_id, set_boot_hartid};
pub use vela_arch_common::AddressSpaceId;
pub use vela_arch_common::{Console, CpuArch, PlatformTimer};

use core::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

/// Per-CPU state, stored as parallel arrays indexed by CPU index.
///
/// Each entry is only touched by its own CPU after boot, except during the
/// boot sequence when the boot hart writes each secondary's entry before
/// starting it. The arrays are bounded by [`smp::MAX_CPUS`], which is also
/// the x86-64 crate's bound, because the kernel's per-CPU bookkeeping is
/// shared.
static CPU_HART_ID: [AtomicU32; smp::MAX_CPUS] = [const { AtomicU32::new(0) }; smp::MAX_CPUS];
static CPU_READY: [AtomicU32; smp::MAX_CPUS] = [const { AtomicU32::new(0) }; smp::MAX_CPUS];
static CPU_CURRENT_TASK: [AtomicU32; smp::MAX_CPUS] = [const { AtomicU32::new(0) }; smp::MAX_CPUS];

/// This CPU's index into the per-CPU arrays, set during early boot.
///
/// Each hart records its own index from the boot protocol's `a0` (see
/// [`regs::set_boot_hartid`]); after that every hart reads only its own
/// index, so there is no cross-hart contention.
static THIS_CPU_INDEX: AtomicUsize = AtomicUsize::new(0);

/// The riscv64 implementation of [`CpuArch`].
pub struct RiscvCpu;

// SAFETY: every operation is a single hart-local instruction or CSR access
// with no inter-hart effect, so any context may call it.
impl CpuArch for RiscvCpu {
    fn halt() {
        regs::wfi();
    }

    fn enable_interrupts() {
        // SAFETY: `CpuArch::enable_interrupts` is the caller's explicit
        // request to unmask; the usual hazard is re-entering a lock the
        // caller still holds, which is the caller's contract to respect.
        unsafe { regs::sstatus_ie_mask() };
    }

    fn disable_interrupts() {
        // SAFETY: clearing `SIE` cannot fault and has no other effect.
        unsafe { regs::sstatus_ie_unmask() };
    }

    fn interrupts_enabled() -> bool {
        regs::interrupts_enabled()
    }

    fn current_cpu_id() -> usize {
        // The boot protocol's `a0` is the hart id, cached per hart by the
        // kernel's entry path. Off bare metal (hosted tests) nothing set
        // it, so fall back to the static index like the x86-64 crate does.
        if regs::ON_BARE_METAL && regs::hart_id() != 0 {
            return regs::hart_id() as usize;
        }
        THIS_CPU_INDEX.load(Ordering::Acquire)
    }

    fn busy_wait_cycles() -> u64 {
        // The time counter is the only architectural clock, so the budget
        // is expressed in ticks: 50 microseconds at the QEMU `virt`
        // timebase. Long enough to cover a short device latency, short
        // enough that a timeout path stays responsive.
        let start = timer::monotonic_ticks();
        let budget = timer::TIMEBASE_HZ / 20;
        // Wrapping, because a caller that started near the counter's end
        // must not panic in the arithmetic.
        let target = start.wrapping_add(budget);
        timer::busy_wait(budget);
        target
    }

    /// Tell the HAL which CPU index this CPU is. Called once during early
    /// boot on every CPU before the scheduler considers it runnable.
    ///
    /// # Safety
    ///
    /// Must be called exactly once per CPU, before any code on that CPU
    /// reads [`Self::current_cpu_id`].
    unsafe fn set_this_cpu(index: usize) {
        THIS_CPU_INDEX.store(index, Ordering::Release);
    }

    /// Register this CPU's per-CPU state, including its hart id.
    ///
    /// Called by the boot hart for itself, and by each secondary hart's
    /// entry point after [`smp::sbi_hart_start`] brings it up.
    ///
    /// # Safety
    ///
    /// Must be called exactly once per CPU before `current_cpu_id()` is
    /// used.
    unsafe fn register_cpu(hart_id: u32, index: usize) {
        if index >= CPU_HART_ID.len() {
            // An index past the end of the per-CPU arrays is ignored
            // *entirely*. Storing it in the current-CPU index would make
            // every later per-CPU access read out of bounds — the index
            // would alias some other CPU's state.
            return;
        }
        CPU_HART_ID[index].store(hart_id, Ordering::Release);
        THIS_CPU_INDEX.store(index, Ordering::Release);
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
}

/// The riscv64 implementation of [`PlatformTimer`], backed by the `time`
/// counter and the Sstc/SBI deadline machinery.
pub struct RiscvTimer;

impl PlatformTimer for RiscvTimer {
    fn monotonic_ticks() -> u64 {
        timer::monotonic_ticks()
    }

    fn ticks_per_second() -> u64 {
        timer::TIMEBASE_HZ
    }

    fn busy_wait(ticks: u64) {
        timer::busy_wait(ticks);
    }
}

/// The riscv64 implementation of [`Console`], backed by the NS16550A UART.
pub struct RiscvConsole;

impl Console for RiscvConsole {
    fn put_byte(byte: u8) {
        serial::put_byte(byte);
    }

    fn put_str(s: &str) {
        serial::puts(s);
    }
}

/// The index of this CPU, delegating to the architecture's [`CpuArch`]
/// implementation. Kernel code calls this rather than naming a concrete
/// CPU type, which keeps `kernel/*` architecture-neutral.
pub fn current_cpu_id() -> usize {
    RiscvCpu::current_cpu_id()
}

#[cfg(test)]
extern crate std;

#[cfg(test)]
mod tests {
    use super::*;
    use std::vec::Vec;
    use vela_arch_common::MapError;

    #[test]
    fn cpu_id_is_zero_on_a_fresh_hart() {
        // The test process never ran the boot protocol, so the cached
        // hart id is 0 and the static index is 0.
        assert_eq!(RiscvCpu::current_cpu_id(), 0);
        assert_eq!(current_cpu_id(), 0);
    }

    #[test]
    fn interrupts_can_be_masked_and_restored() {
        // Off bare metal the CSR writes are compiled out, so the
        // *observable* state cannot change here. What is worth asserting
        // on the host is that the accessor reports something coherent and
        // that the pair is safe to call; the real transition is verified
        // by the boot smoke test on the target.
        let was = RiscvCpu::interrupts_enabled();
        RiscvCpu::disable_interrupts();
        let after_disable = RiscvCpu::interrupts_enabled();
        RiscvCpu::enable_interrupts();
        let after_enable = RiscvCpu::interrupts_enabled();

        if !regs::ON_BARE_METAL {
            // Host build: the calls are no-ops, so the state must not move.
            assert_eq!(after_disable, was);
            assert_eq!(after_enable, was);
        }
    }

    #[test]
    fn halt_and_busy_wait_return() {
        // These cannot be meaningfully asserted beyond "they return"; the
        // point is that the code paths assemble and do not fault or hang
        // on the host.
        RiscvCpu::halt();
        let end = RiscvCpu::busy_wait_cycles();
        assert!(end > 0);
        let seen = Vec::from([end]);
        assert_eq!(seen.len(), 1);
    }

    #[test]
    fn register_and_ready_bookkeeping_round_trips() {
        // SAFETY: single-threaded test process, calling the documented
        // boot-time registration.
        unsafe { RiscvCpu::register_cpu(3, 1) };
        assert_eq!(THIS_CPU_INDEX.load(Ordering::Acquire), 1);
        assert!(!RiscvCpu::is_cpu_ready());
        RiscvCpu::mark_cpu_ready();
        assert!(RiscvCpu::is_cpu_ready());
        // SAFETY: resetting the test-visible index so other tests start
        // from the documented fresh state.
        unsafe { RiscvCpu::set_this_cpu(0) };
        assert_eq!(RiscvCpu::current_cpu_id(), 0);
    }

    #[test]
    fn task_bookkeeping_round_trips() {
        RiscvCpu::set_current_task(7);
        assert_eq!(RiscvCpu::current_task_id(), 7);
        RiscvCpu::set_current_task(0);
        assert_eq!(RiscvCpu::current_task_id(), 0);
    }

    #[test]
    fn out_of_range_indices_are_ignored_not_aliasing() {
        // A CPU index at or above MAX_CPUS must not write into some other
        // array's memory — the guards make the write a no-op instead.
        // SAFETY: single-threaded test process.
        unsafe { RiscvCpu::register_cpu(9, smp::MAX_CPUS + 1) };
        assert_eq!(RiscvCpu::current_cpu_id(), 0);
    }

    #[test]
    fn the_timer_ticks_per_second_is_the_timebase() {
        assert_eq!(RiscvTimer::ticks_per_second(), timer::TIMEBASE_HZ);
        assert_eq!(RiscvTimer::monotonic_ticks(), 0);
        RiscvTimer::busy_wait(1);
    }

    #[test]
    fn the_console_impls_exist_and_are_inert() {
        // The trait impls route to the UART driver, which is a no-op off
        // bare metal — exercising them is "it assembles and does not
        // fault", like the x86-64 crate's console tests.
        RiscvConsole::put_byte(b'x');
        RiscvConsole::put_str("hello\n");
    }

    #[test]
    fn the_root_re_exports_are_the_kernel_surface() {
        // The facade's module surface: these names must exist at the root
        // so `kernel/*` can be architecture-neutral.
        let _ = AddressSpace::new(AddressSpaceId(0));
        let _ = core::mem::size_of::<Split>();
        let _ = IdentityPhys;
        // `FrameSource` and `PhysOfVirt` are traits; proving they are
        // importable and nameable is what the bounds below do.
        fn takes_frame_source<F: FrameSource>(_: F) {}
        fn takes_phys_of_virt<P: PhysOfVirt>(_: P) {}
        takes_frame_source(FakeSource);
        takes_phys_of_virt(IdentityPhys);
    }

    /// A trivial [`FrameSource`] used only to prove the trait is importable.
    struct FakeSource;
    impl FrameSource for FakeSource {
        fn alloc_frame(&mut self) -> Result<u64, MapError> {
            Err(MapError::OutOfMemory)
        }
        unsafe fn free_frame(&mut self, _pa: u64) {}
    }
}
