//! x86-64 port I/O, control registers, and CPU flags.
//!
//! Everything that touches hardware directly lives behind `#[inline]` asm
//! here, so the rest of the HAL can be reviewed as ordinary safe Rust with a
//! small, auditable unsafe surface.
//!
//! # Safety policy
//!
//! The primitives in this module are `unsafe` because they are `unsafe` at
//! the ISA level: a port number can address a device the caller knows nothing
//! about, and a control-register write can change the behaviour of the whole
//! machine. Callers are expected to be thin, named wrappers — one per device —
//! so that the set of port numbers actually used is greppable.
//!
//! Out-of-line (`non-inline`) `asm!` is used deliberately. Inline asm cannot
//! fall through to the value of `in`/`out` operands, so the only way to get
//! "read a port" to compile is an out-of-line block with `options(nomem,
//! nostack, preserves_flags)` and an explicit early return.

use core::arch::asm;
use core::sync::atomic::{AtomicU8, Ordering};

/// `IN AL, imm8` and friends.
///
/// # Safety
///
/// `port` must belong to a device the caller owns; reads can have device
/// side effects.
#[inline]
pub unsafe fn inb(port: u16) -> u8 {
    if !ON_BARE_METAL {
        return 0;
    }
    let value: u8;
    // SAFETY: the caller guarantees `port` is a port this device owns and that
    // reading it has no side effects beyond returning a byte.
    unsafe {
        asm!(
            "in al, dx",
            in("dx") port,
            out("al") value,
            options(nomem, nostack, preserves_flags)
        );
    }
    value
}

/// `IN AX, imm8` and friends.
///
/// # Safety
///
/// `port` must belong to a device the caller owns; reads can have device
/// side effects.
#[inline]
pub unsafe fn inw(port: u16) -> u16 {
    if !ON_BARE_METAL {
        return 0;
    }
    let value: u16;
    // SAFETY: as [`inb`], for a 16-bit port.
    unsafe {
        asm!(
            "in ax, dx",
            in("dx") port,
            out("ax") value,
            options(nomem, nostack, preserves_flags)
        );
    }
    value
}

/// `IN EAX, imm8` and friends.
///
/// # Safety
///
/// `port` must belong to a device the caller owns; reads can have device
/// side effects.
#[inline]
pub unsafe fn inl(port: u16) -> u32 {
    if !ON_BARE_METAL {
        return 0;
    }
    let value: u32;
    // SAFETY: as [`inb`], for a 32-bit port.
    unsafe {
        asm!(
            "in eax, dx",
            in("dx") port,
            out("eax") value,
            options(nomem, nostack, preserves_flags)
        );
    }
    value
}

/// `OUT imm8, AL` and friends.
///
/// # Safety
///
/// `port` must belong to a device the caller owns; writes can reconfigure or
/// corrupt hardware.
#[inline]
pub unsafe fn outb(port: u16, value: u8) {
    if !ON_BARE_METAL {
        return;
    }
    // SAFETY: the caller guarantees `port` is a port this device owns.
    unsafe {
        asm!(
            "out dx, al",
            in("dx") port,
            in("al") value,
            options(nomem, nostack, preserves_flags)
        );
    }
}

/// `OUT imm8, AX` and friends.
///
/// # Safety
///
/// `port` must belong to a device the caller owns; writes can reconfigure or
/// corrupt hardware.
#[inline]
pub unsafe fn outw(port: u16, value: u16) {
    if !ON_BARE_METAL {
        return;
    }
    // SAFETY: as [`outb`], for a 16-bit port.
    unsafe {
        asm!(
            "out dx, ax",
            in("dx") port,
            in("ax") value,
            options(nomem, nostack, preserves_flags)
        );
    }
}

/// `OUT imm8, EAX` and friends.
///
/// # Safety
///
/// `port` must belong to a device the caller owns; writes can reconfigure or
/// corrupt hardware.
#[inline]
pub unsafe fn outl(port: u16, value: u32) {
    if !ON_BARE_METAL {
        return;
    }
    // SAFETY: as [`outb`], for a 32-bit port.
    unsafe {
        asm!(
            "out dx, eax",
            in("dx") port,
            in("eax") value,
            options(nomem, nostack, preserves_flags)
        );
    }
}

/// Whether privileged instructions may be issued.
///
/// Reading a control register traps in ring 3, so the same rule that governs
/// TLB maintenance applies here: off bare metal these accessors return a
/// harmless zero instead of faulting, which is what lets the HAL be unit
/// tested on the host.
#[cfg(target_os = "none")]
pub const ON_BARE_METAL: bool = true;
/// Off bare metal the constant is `false`; see the note above.
#[cfg(not(target_os = "none"))]
pub const ON_BARE_METAL: bool = false;

/// Read CR0.
#[inline]
pub fn read_cr0() -> u64 {
    if !ON_BARE_METAL {
        return 0;
    }
    let value: u64;
    // SAFETY: `mov r64, CR0` is unconditionally available in long mode.
    unsafe {
        asm!("mov {}, cr0", out(reg) value, options(nomem, nostack, preserves_flags));
    }
    value
}

/// Read CR2 — the address of the last page fault.
#[inline]
pub fn read_cr2() -> u64 {
    if !ON_BARE_METAL {
        return 0;
    }
    let value: u64;
    // SAFETY: `mov r64, CR2` is unconditionally available in long mode.
    unsafe {
        asm!("mov {}, cr2", out(reg) value, options(nomem, nostack, preserves_flags));
    }
    value
}

/// Read CR3 — the physical base of the PML4.
#[inline]
pub fn read_cr3() -> u64 {
    if !ON_BARE_METAL {
        return 0;
    }
    let value: u64;
    // SAFETY: `mov r64, CR3` is unconditionally available in long mode.
    unsafe {
        asm!("mov {}, cr3", out(reg) value, options(nomem, nostack, preserves_flags));
    }
    value
}

/// Write CR3, loading a new address space.
///
/// A bare `mov cr3` discards every TLB entry, which is what address-space
/// switches need. Later milestones replace this with the `PCID` path once
/// there is more than one CPU.
///
/// # Safety
///
/// `pml4` must be the physical base of a PML4 whose page tables are fully
/// built, and all CR3 bits except the physical address must be zero.
#[inline]
pub unsafe fn write_cr3(pml4: u64) {
    if !ON_BARE_METAL {
        return;
    }
    // SAFETY: the caller guarantees `pml4` is a valid page-table base.
    unsafe {
        asm!("mov cr3, {}", in(reg) pml4, options(nostack, preserves_flags));
    }
}

/// Enable PAE. Required for long mode; left enabled afterwards.
#[inline]
pub fn enable_pae() {
    if !ON_BARE_METAL {
        return;
    }
    let cr0 = read_cr0();
    // SAFETY: setting CR0.PAE is a documented, unconditional operation, and it
    // is a prerequisite of long mode which we are already running in.
    unsafe { write_cr0(cr0 | (1 << 5)) };
}

/// Write CR0.
///
/// # Safety
///
/// `value` must preserve the mode bits required by the current paging and
/// privilege configuration.
#[inline]
pub unsafe fn write_cr0(value: u64) {
    // SAFETY: the caller passes the CR0 value explicitly.
    unsafe {
        asm!("mov cr0, {}", in(reg) value, options(nostack, preserves_flags));
    }
}

/// Enable the write-protect bit, so ring 0 cannot accidentally write to
/// read-only mappings.
///
/// This is a policy the rest of the kernel depends on: `kernel_rx` mappings are
/// only meaningful if a stray store traps.
#[inline]
pub fn enable_write_protect() {
    if !ON_BARE_METAL {
        return;
    }
    let cr0 = read_cr0();
    // SAFETY: as [`enable_pae`].
    unsafe { write_cr0(cr0 | (1 << 16)) };
}

/// The extended flags register: the bits `LAHF`/`SAHF` cannot reach.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rflags(u64);

impl Rflags {
    /// Interrupt enable flag.
    pub const IF: u64 = 1 << 9;
    /// Direction flag: clear for string instructions to count up.
    pub const DF: u64 = 1 << 10;
    /// Nested task flag.
    pub const NT: u64 = 1 << 14;

    /// Current value.
    #[inline]
    pub fn read() -> Self {
        let value: u64;
        // SAFETY: `pushfq`/`pop` is unprivileged and always available. The
        // `pushfq` cannot be reordered against the `pop` because `asm!` treats
        // the whole block as one side-effecting unit unless told otherwise.
        unsafe {
            asm!("pushfq", "pop {}", out(reg) value, options(nomem, nostack));
        }
        Self(value & 0x0000_44C5)
    }

    /// True when interrupts are unmasked at the CPU level.
    #[inline]
    pub const fn interrupts_enabled(self) -> bool {
        self.0 & Self::IF != 0
    }
}

/// `CLI` — mask hardware interrupts.
///
/// A no-op off bare metal: `CLI` is a privileged instruction, and on Linux the
/// `cli`/`sti` pair in user mode is a classic way to get a `SIGILL` that the
/// test runner reports as a crash.
#[inline]
pub fn cli() {
    if !ON_BARE_METAL {
        return;
    }
    // SAFETY: `cli` is unprivileged and has no memory effect.
    unsafe { asm!("cli", options(nomem, nostack, preserves_flags)) }
}

/// `STI` — unmask hardware interrupts.
///
/// # Safety
///
/// Nothing forbids this, but calling it while holding a lock can deadlock the
/// machine: an interrupt taken immediately after the `sti` may re-enter the
/// critical section that the caller believes it still holds.
#[inline]
pub unsafe fn sti() {
    if !ON_BARE_METAL {
        return;
    }
    // SAFETY: the caller accepts the usual "enable interrupts late" discipline.
    unsafe { asm!("sti", options(nomem, nostack, preserves_flags)) }
}

/// Enable interrupts if they were enabled on entry.
///
/// The canonical "restore flags" primitive for an interrupt handler that has no
/// business changing the interrupt-enable state.
#[inline]
pub fn restore_interrupts(was_enabled: bool) {
    if was_enabled {
        // SAFETY: we only re-enable when the interrupted code had them enabled,
        // so no critical section is silently left open.
        unsafe { sti() };
    }
}

/// `HLT` — halt until the next interrupt.
///
/// A no-op off bare metal. `HLT` in ring 3 is undefined behaviour on Linux —
/// the kernel may terminate the process — so the host test runner must never
/// reach it. On bare metal this is the correct way to idle until the next
/// timer interrupt.
#[inline]
pub fn hlt() {
    if !ON_BARE_METAL {
        return;
    }
    // SAFETY: unprivileged; the caller must tolerate waking on an interrupt.
    unsafe { asm!("hlt", options(nomem, nostack, preserves_flags)) }
}

/// `PAUSE` — yield a pipelined core to a sibling hyperthread.
#[inline]
pub fn pause() {
    // SAFETY: unprivileged hint with no architectural effect.
    unsafe { asm!("pause", options(nomem, nostack, preserves_flags)) }
}

/// Spin for at least `iterations` `PAUSE`s.
#[inline]
pub fn spin_hint(iterations: u64) {
    for _ in 0..iterations {
        pause();
    }
}

/// `CPUID` leaf 0 — the vendor string and the maximum supported leaf.
#[inline]
pub fn cpuid_max_leaf() -> (u32, [u8; 12]) {
    let (eax, ebx, ecx, edx) = cpuid(0, 0);
    let mut vendor = [0u8; 12];
    vendor[0..4].copy_from_slice(&ebx.to_le_bytes());
    vendor[4..8].copy_from_slice(&edx.to_le_bytes());
    vendor[8..12].copy_from_slice(&ecx.to_le_bytes());
    (eax, vendor)
}

/// `CPUID` with a leaf and subleaf.
#[inline]
pub fn cpuid(leaf: u32, subleaf: u32) -> (u32, u32, u32, u32) {
    let (eax, ebx, ecx, edx): (u32, u32, u32, u32);
    // SAFETY: `CPUID` is available on every x86-64 CPU and cannot fault.
    //
    // RBX is special: it is the PIC register on 32-bit targets, so LLVM
    // reserves it and refuses it as an operand. The standard workaround is to
    // spill it to a scratch register around the instruction. The scratch
    // register is not named, so LLVM is free to pick one that does not collide
    // with the inputs or outputs — which is exactly the property we need.
    unsafe {
        asm!(
            "mov {tmp:r}, rbx",
            "cpuid",
            "xchg {tmp:r}, rbx",
            inout("eax") leaf => eax,
            inout("ecx") subleaf => ecx,
            out("edx") edx,
            tmp = lateout(reg) ebx,
        );
    }
    (eax, ebx, ecx, edx)
}

/// `RDTSC` — the processor's cycle counter, monotonic per core.
#[inline]
pub fn rdtsc() -> u64 {
    let lo: u32;
    let hi: u32;
    // SAFETY: `RDTSC` is available on every x86-64 CPU and cannot fault.
    unsafe {
        asm!(
            "rdtsc",
            out("eax") lo,
            out("edx") hi,
            options(nomem, nostack, preserves_flags)
        );
    }
    ((hi as u64) << 32) | (lo as u64)
}

/// `RDTSCP` — `RDTSC` plus the owning CPU and an auxiliary value.
///
/// Unlike `RDTSC` this is ordered against earlier loads, which is what makes it
/// the right choice for measuring elapsed time across a VM exit.
/// `RDTSC` fenced on both sides, for measuring elapsed time.
///
/// # Why not `RDTSCP`
///
/// `RDTSCP` is *not* baseline x86-64: it is an optional CPU feature (leaf
/// `0x80000001`, `EDX` bit 27). It is available on every physical CPU from
/// the last two decades, but **not** on every virtual CPU — QEMU's default
/// `q35` CPU model does not expose it, and a kernel that issues `RDTSCP`
/// unconditionally dies with `#UD` the moment it first reads the clock.
///
/// That failure is nasty to diagnose: the fault lands deep inside the timer
/// calibration, the register dump points at an instruction with no source
/// line, and the kernel dies on every machine whose CPU model merely differs
/// from the one it was tested on.
///
/// `RDTSC` *is* baseline, and `LFENCE` provides the ordering guarantee the
/// `RDTSCP` was wanted for: `LFENCE` before the read orders all earlier loads
/// against it, and `LFENCE` after it orders the read against all later loads.
/// The only thing lost is the auxiliary value — the CPU id and `TSC_AUX` —
/// which a single-core kernel has no use for.
///
/// # Why the fences are two
///
/// One fence only orders in one direction. A load that started before the
/// `RDTSC` could still retire after it, which is exactly the skew a timing
/// measurement must not have. Fencing on both sides closes the window.
#[inline]
pub fn rdtsc_fenced() -> u64 {
    let lo: u32;
    let hi: u32;
    // SAFETY: `LFENCE` and `RDTSC` are available on every x86-64 CPU and
    // cannot fault. `LFENCE` is unprivileged and has no memory effect beyond
    // ordering.
    unsafe {
        asm!(
            "lfence",
            "rdtsc",
            "lfence",
            out("eax") lo,
            out("edx") hi,
            options(nomem, nostack, preserves_flags)
        );
    }
    ((hi as u64) << 32) | (lo as u64)
}

/// The extended `CPUID` leaf that reports the `RDTSCP` feature bit.
///
/// Leaf `0x8000_0000` is the *maximum* extended leaf; a CPU that reports a
/// maximum below `0x8000_0001` does not implement leaf `0x8000_0001` at all,
/// and reading it would return whatever the last leaf left in `EAX`.
pub const CPUID_EXTENDED_8000_0000: u32 = 0x8000_0000;

/// The extended `CPUID` leaf that carries the `RDTSCP` feature bit.
pub const CPUID_EXTENDED_8000_0001: u32 = 0x8000_0001;

/// `RDTSCP` support bit in `CPUID.8000_0001:EDX`.
///
/// Bit 27 is `RDTSCP`. It is the only bit of that leaf the kernel consults.
pub const CPUID_8000_0001_EDX_RDTSCP: u32 = 1 << 27;

/// Cached result of the `RDTSCP` feature query.
///
/// The encoding is deliberate:
///
/// * `0` — not yet queried.
/// * `1` — queried, and `RDTSCP` is **not** available.
/// * `2` — queried, and `RDTSCP` **is** available.
///
/// A plain `AtomicBool` cannot express "not yet known", and a plain `bool`
/// would be a data race the moment a second CPU ever looked at it. The
/// tri-state lets the query run exactly once without a lock, and the
/// `Acquire`/`Release` pair makes the answer visible to every other CPU that
/// reads it afterwards.
static RDTSCP_SUPPORTED: AtomicU8 = AtomicU8::new(0);

/// Whether this CPU implements the `RDTSCP` instruction.
///
/// # Why this must be checked rather than assumed
///
/// `RDTSCP` is **not** baseline x86-64. It is an optional CPU feature
/// reported by `CPUID.8000_0001:EDX` bit 27, and it is absent on plenty of
/// perfectly ordinary virtual CPUs — QEMU's default `qemu64` model, for
/// instance, does not implement it.
///
/// Executing `RDTSCP` where it is unsupported raises `#UD` — an *invalid
/// opcode* exception. On a machine whose exception handling is not yet
/// established, that `#UD` becomes a triple fault and the machine dies with
/// no diagnostic at all. That is exactly how a kernel that "just reads the
/// clock" can brick a virtual machine on the very first timer calibration.
///
/// So the kernel never issues `RDTSCP` without asking first, and asks exactly
/// once: the answer is cached in a static so the `CPUID` leaf is not
/// re-read on every clock read.
///
/// # The fallback
///
/// When the feature is absent, callers use [`rdtsc_fenced`], which is
/// baseline and needs no feature bit. The only thing lost is the auxiliary
/// value (`TSC_AUX`), which a single-core kernel has no use for.
pub fn has_rdtscp() -> bool {
    match RDTSCP_SUPPORTED.load(Ordering::Acquire) {
        1 => false,
        2 => true,
        _ => query_rdtscp(),
    }
}

/// Run the `CPUID` feature query exactly once and cache the answer.
///
/// The compare-and-swap makes the query idempotent under concurrency: if two
/// CPUs race, one performs the query and the other sees the cached answer.
/// The `Acquire` on the load and the `Release` on the store give the second
/// CPU the happens-before edge it needs to see the first CPU's result.
fn query_rdtscp() -> bool {
    let supported = detect_rdtscp();
    let encoded = if supported { 2 } else { 1 };
    // `compare_exchange` returns the *previous* value. If it was already
    // non-zero, another CPU answered first and its answer is the one to
    // believe — the query is deterministic, so both answers agree, but
    // honouring the first writer keeps the "exactly once" property literal.
    match RDTSCP_SUPPORTED.compare_exchange(0, encoded, Ordering::AcqRel, Ordering::Acquire) {
        Ok(_) => supported,
        Err(previous) => previous == 2,
    }
}

/// Ask `CPUID` directly whether `RDTSCP` is implemented.
///
/// Split out from [`query_rdtscp`] so the detection logic can be tested on
/// the host without touching the cached static.
fn detect_rdtscp() -> bool {
    // `CPUID` leaf `0x8000_0000` reports the highest extended leaf the CPU
    // implements. If that is below `0x8000_0001`, the feature leaf does not
    // exist and reading it would be meaningless.
    let (max_extended, _, _, _) = cpuid(CPUID_EXTENDED_8000_0000, 0);
    if max_extended < CPUID_EXTENDED_8000_0001 {
        return false;
    }
    let (_, _, _, edx) = cpuid(CPUID_EXTENDED_8000_0001, 0);
    edx & CPUID_8000_0001_EDX_RDTSCP != 0
}

/// Read the timestamp counter, using `RDTSCP` when the CPU has it.
///
/// # This function cannot trap
///
/// `RDTSCP` is an optional CPU feature, and executing it on a CPU that lacks
/// it raises `#UD`. This function checks the feature first and falls back to
/// the baseline [`rdtsc_fenced`] when it is absent, so it is safe to call on
/// any x86-64 CPU.
///
/// When `RDTSCP` is present it is used, because it is ordered against
/// earlier loads *and* it returns the owning CPU's identifier. When it is
/// absent the baseline reader is used instead, which gives the same ordering
/// guarantee through `LFENCE` and costs nothing but the auxiliary value.
pub fn tsc() -> u64 {
    if has_rdtscp() {
        // SAFETY: `has_rdtscp` established that the CPU implements the
        // `rdtscp` feature, so the instruction cannot fault.
        unsafe { rdtscp_raw().0 }
    } else {
        rdtsc_fenced()
    }
}

/// `RDTSCP` — `TSC` plus the owning CPU and an auxiliary value.
///
/// Unlike `RDTSC` this is ordered against earlier loads, which is what makes
/// it the right choice for measuring elapsed time across a VM exit.
///
/// # This function never traps
///
/// `RDTSCP` is **not** baseline x86-64 — it is the optional `rdtscp` CPU
/// feature reported by `CPUID.8000_0001:EDX` bit 27. QEMU's default `qemu64`
/// CPU model does not implement it, and executing the instruction there
/// raises `#UD`.
///
/// So this function *checks* before it executes. When the feature is absent
/// it falls back to the baseline [`rdtsc_fenced`] and reports an auxiliary
/// value of zero. A caller therefore cannot accidentally triple-fault the
/// machine by reading the clock.
///
/// # The fallback's cost
///
/// The only thing lost without `RDTSCP` is the auxiliary value. The ordering
/// guarantee survives, because `LFENCE` on both sides of `RDTSC` provides the
/// same ordering `RDTSCP` gives.
#[inline]
pub fn rdtscp() -> (u64, u32) {
    if has_rdtscp() {
        // SAFETY: `has_rdtscp` established that the CPU implements the
        // `rdtscp` feature, so the instruction cannot fault.
        unsafe { rdtscp_raw() }
    } else {
        (rdtsc_fenced(), 0)
    }
}

/// The raw `RDTSCP` instruction, with no feature check.
///
/// # Safety
///
/// The CPU **must** implement the `rdtscp` feature. Executing this function
/// on a CPU that does not raises `#UD` — an invalid-opcode exception, which
/// on a machine whose exception handling is not yet established becomes a
/// triple fault with no diagnostic.
///
/// Only [`rdtscp`] and [`tsc`] call this, and both check the feature first.
#[inline]
unsafe fn rdtscp_raw() -> (u64, u32) {
    let lo: u32;
    let hi: u32;
    let aux: u32;
    // SAFETY: the caller guarantees the CPU implements the `rdtscp` feature,
    // so the instruction is architecturally defined and cannot fault.
    unsafe {
        asm!(
            "rdtscp",
            out("eax") lo,
            out("edx") hi,
            out("ecx") aux,
            options(nomem, nostack)
        );
    }
    (((hi as u64) << 32) | (lo as u64), aux)
}

/// Read the current stack pointer.
///
/// # Why the kernel needs it
///
/// The CPU does not push `RSP` and `SS` when it takes a **double fault**:
/// it cannot trust the stack it is running on, so it does not record it. The
/// only honest way to report the stack a double fault happened on is to
/// read the register directly.
#[inline]
pub fn read_rsp() -> u64 {
    let value: u64;
    // SAFETY: reading `RSP` has no effect on machine state.
    unsafe { asm!("mov {}, rsp", out(reg) value, options(nomem, nostack, preserves_flags)) };
    value
}

/// Read the current instruction pointer.
///
/// # Why the kernel needs it
///
/// The same reason as [`read_rsp`]: on a double fault the CPU pushes only
/// the error code, `RIP`, `CS` and `RFLAGS`, and the report wants to say
/// where the machine was.
#[inline]
pub fn read_rip() -> u64 {
    let value: u64;
    // SAFETY: reading `RIP` has no effect on machine state.
    unsafe { asm!("mov {}, rip", out(reg) value, options(nomem, nostack, preserves_flags)) };
    value
}

/// Read a model-specific register.
#[inline]
pub fn rdmsr(msr: u32) -> u64 {
    let (lo, hi): (u32, u32);
    // SAFETY: the caller passes a valid MSR number.
    unsafe {
        asm!(
            "rdmsr",
            in("ecx") msr,
            out("eax") lo,
            out("edx") hi,
            options(nomem, nostack, preserves_flags)
        );
    }
    ((hi as u64) << 32) | (lo as u64)
}

/// Write a model-specific register.
///
/// # Safety
///
/// `msr` must exist on the target CPU, and `value` must be meaningful for
/// that MSR.
#[inline]
pub unsafe fn wrmsr(msr: u32, value: u64) {
    // SAFETY: the caller passes a valid MSR number and value.
    unsafe {
        asm!(
            "wrmsr",
            in("ecx") msr,
            in("eax") value as u32,
            in("edx") (value >> 32) as u32,
            options(nostack, preserves_flags)
        );
    }
}

/// Write a model-specific register, taking the value as a `u64`.
///
/// This is the same instruction as [`wrmsr`], but with the 64-bit value
/// split into the two 32-bit halves here rather than at every call site. The
/// split is the part that is easy to get wrong, so it is done once.
///
/// # Safety
///
/// The caller passes a valid MSR number, and a value that is meaningful for
/// that MSR.
#[inline]
pub unsafe fn write_msr(msr: u32, value: u64) {
    // SAFETY: the caller guarantees the MSR number is valid and the value is
    // meaningful for it; `wrmsr` cannot fault on a valid MSR.
    unsafe { wrmsr(msr, value) }
}

/// `LFENCE` — order earlier loads against later loads and stores.
#[inline]
pub fn lfence() {
    // SAFETY: unprivileged serialisation hint.
    unsafe { asm!("lfence", options(nomem, nostack, preserves_flags)) }
}

/// `MFENCE` — full fence.
#[inline]
pub fn mfence() {
    // SAFETY: unprivileged serialisation hint.
    unsafe { asm!("mfence", options(nomem, nostack, preserves_flags)) }
}

/// Read a CR MSR — only valid on Intel.
#[inline]
pub fn read_cr(msr: u32) -> u64 {
    let (lo, hi): (u32, u32);
    // SAFETY: `IA32_CR_*` MSRs are readable with RDMSR on any CPU; reading an
    // unsupported one returns garbage rather than faulting.
    //
    // The 32-bit halves are moved with explicit `e` register names, so the
    // compiler cannot allocate a high-half register that `mov` would then
    // truncate.
    unsafe {
        asm!(
            "mov {lo:e}, eax",
            "mov {hi:e}, edx",
            "rdmsr",
            in("ecx") msr,
            lo = lateout(reg) lo,
            hi = lateout(reg) hi,
            options(nomem, nostack, preserves_flags)
        );
    }
    ((hi as u64) << 32) | (lo as u64)
}

/// Write a CR MSR — only valid on Intel.
///
/// # Safety
///
/// `msr` must identify a writable control-register MSR, and `value` must
/// preserve required mode bits.
#[inline]
pub unsafe fn write_cr(msr: u32, value: u64) {
    // SAFETY: the caller passes a valid MSR number; writing an unsupported MSR
    // is undefined on Intel, so the caller must check `CPUID.01H:ECX.PDCM`.
    unsafe {
        asm!(
            "mov eax, {lo:e}",
            "mov edx, {hi:e}",
            "wrmsr",
            in("ecx") msr,
            lo = in(reg) value as u32,
            hi = in(reg) (value >> 32) as u32,
            options(nostack, preserves_flags)
        );
    }
}

/// Read `MSR_EFER` — the extended feature enable register.
#[inline]
pub fn read_efer() -> u64 {
    rdmsr(MSR_EFER)
}

/// Write `MSR_EFER`.
///
/// # Safety
///
/// `value` must only enable features the CPU and kernel have validated.
#[inline]
pub unsafe fn write_efer(value: u64) {
    // SAFETY: the caller assembles the value; see [`efers`].
    unsafe { wrmsr(MSR_EFER, value) }
}

/// `IA32_EFER`.
pub const MSR_EFER: u32 = 0xC000_0080;

/// `IA32_STAR` — the legacy segment selectors for `syscall`.
pub const MSR_STAR: u32 = 0xC000_0081;
/// `IA32_LSTAR` — the `syscall` entry point.
pub const MSR_LSTAR: u32 = 0xC000_0082;
/// `IA32_SFMASK` — the RFLAGS mask applied on `syscall` entry.
pub const MSR_SFMASK: u32 = 0xC000_0084;
/// `IA32_FS_BASE` — the base of the `fs` segment.
pub const MSR_FS_BASE: u32 = 0xC000_0100;
/// `IA32_KERNEL_GS_BASE` — the base used for `swapgs`.
pub const MSR_KERNEL_GS_BASE: u32 = 0xC000_0102;

/// `IA32_CR0.PE`.
pub const CR0_PE: u64 = 1 << 0;
/// `IA32_CR0.PG`.
pub const CR0_PG: u64 = 1 << 31;
/// `IA32_CR4.PAE`.
pub const CR4_PAE: u64 = 1 << 5;
/// `IA32_CR4.OSFXSR` — SSE state save area.
pub const CR4_OSFXSR: u64 = 1 << 9;
/// `IA32_CR4.PCIDE` — PCID support.
pub const CR4_PCIDE: u64 = 1 << 17;

/// `MSR_EFER` bit definitions.
pub mod efers {
    /// Long mode enable.
    pub const LME: u64 = 1 << 8;
    /// Long mode active.
    pub const LMA: u64 = 1 << 10;
    /// No-execute support.
    pub const NXE: u64 = 1 << 11;
}

/// Read CR4.
#[inline]
pub fn read_cr4() -> u64 {
    if !ON_BARE_METAL {
        return 0;
    }
    let value: u64;
    // SAFETY: `mov r64, CR4` is unconditionally available in long mode.
    unsafe {
        asm!("mov {}, cr4", out(reg) value, options(nomem, nostack, preserves_flags));
    }
    value
}

/// Write CR4.
///
/// # Safety
///
/// `value` must preserve required paging and feature bits and must only
/// enable CPU features the kernel has prepared.
#[inline]
pub unsafe fn write_cr4(value: u64) {
    if !ON_BARE_METAL {
        return;
    }
    // SAFETY: the caller passes the CR4 value explicitly.
    unsafe {
        asm!("mov cr4, {}", in(reg) value, options(nostack, preserves_flags));
    }
}

/// `CLI` + the flags needed by [`set_if_flag`] in one step.
#[inline]
pub fn cli_and_capture() -> u64 {
    cli();
    Rflags::read().0
}

/// Set or clear the interrupt-enable flag.
///
/// Off bare metal this is a no-op: the host's interrupt state is owned by the
/// operating system, and touching it from a library would be a bug in the
/// library, not in the caller.
#[inline]
pub fn set_if_flag(enable: bool) {
    if !ON_BARE_METAL {
        return;
    }
    let flags = Rflags::read().0;
    let updated = if enable {
        flags | Rflags::IF
    } else {
        flags & !Rflags::IF
    };
    if updated != flags {
        // SAFETY: writing RFLAGS through push/pop is unprivileged; only IF
        // changed, and the caller explicitly asked for it.
        unsafe {
            asm!(
                "push {value}",
                "popfq",
                value = in(reg) updated,
                options(nomem, nostack),
            );
        }
    }
}

/// `nop` with the CPU-side implication spelled out for the JIT-less kernel:
/// these platforms do not speculatively execute across a page-table walk, so
/// a missing TLB flush is a correctness bug rather than a security bug. VelaOS
/// therefore does not enable SMEP/SMAP speculatively; it relies on eager
/// invalidation.
///
/// Documenting the assumption rather than enforcing it, because the report
/// asks for W^X and NXE — both of which *are* enforced.
pub const TLB_INVALIDATION_IS_EAGER: bool = true;
/// The operand `LGDT` and `LIDT` read.
///
/// The layout is architectural and byte-exact: a 16-bit byte limit followed
/// by a 64-bit linear base address. It is `packed` because the limit is
/// only 2 bytes wide and the base is not 2-byte aligned.
#[repr(C, packed)]
pub struct DescriptorTablePointer {
    /// The byte offset of the last byte of the table.
    pub limit: u16,
    /// The linear address of the first byte of the table.
    pub base: u64,
}

/// Load the global descriptor table.
///
/// # Safety
///
/// The table `ptr` describes must stay valid for as long as the CPU may
/// reload a segment selector from it, and the caller must have interrupts
/// disabled: between this instruction and the reload of `CS` the CPU still
/// holds the *old* descriptor, and an interrupt taken in that window would
/// return through a selector the new table may not contain.
#[inline]
pub unsafe fn lgdt(ptr: DescriptorTablePointer) {
    if !ON_BARE_METAL {
        return;
    }
    // SAFETY: the caller guarantees the table behind `ptr` is valid and
    // outlives every use of the selectors it holds.
    unsafe {
        asm!(
            "lgdt [{ptr:r}]",
            ptr = in(reg) &ptr,
            options(readonly, nostack, preserves_flags),
        );
    }
}

/// Load the task register with the TSS descriptor named by `selector`.
///
/// # Why a selector and not a descriptor
///
/// `LTR` takes a *selector*, not a descriptor. The 16-byte system descriptor
/// itself lives in the GDT, and the CPU fetches it from there — which is why
/// the TSS descriptor must be in the GDT before `LTR` runs, and why the
/// selector must name the descriptor's index with RPL 0.
///
/// # Safety
///
/// `selector` must name a present TSS system descriptor in the GDT that is
/// currently loaded, and that descriptor's TSS must stay valid for the life
/// of the CPU. The caller must have interrupts disabled: the CPU marks the
/// TSS busy, and a second `LTR` on a busy TSS raises `#GP`.
#[inline]
pub unsafe fn load_tr(selector: u16) {
    if !ON_BARE_METAL {
        return;
    }
    // SAFETY: the caller guarantees the selector names a present TSS
    // descriptor in the loaded GDT whose TSS outlives the CPU.
    unsafe {
        asm!(
            "ltr {selector:x}",
            selector = in(reg) selector as u64,
            options(nomem, nostack, preserves_flags),
        );
    }
}

/// Read the current code segment selector.
#[inline]
pub fn read_cs() -> u16 {
    if !ON_BARE_METAL {
        return 0;
    }
    let value: u16;
    // SAFETY: reading `CS` has no effect on machine state.
    unsafe {
        asm!("mov {value:x}, cs", value = out(reg) value, options(nomem, nostack, preserves_flags));
    }
    value
}

/// Reload the code segment from the GDT that was just installed.
///
/// # Why this exists
///
/// The CPU *caches* the descriptor behind `CS`. Installing a new GDT with
/// `LGDT` does not change the descriptor in force, so the kernel would keep
/// running on the firmware's descriptor — and every far return would reload
/// `CS` from the *new* table, where the firmware's selector may name
/// something else entirely.
///
/// `CS` cannot be loaded with an ordinary `mov`: x86-64 requires the
/// instruction pointer and the code segment to change *together*. The
/// architecturally defined way to do that is a far return, which pops the
/// target and the selector in one step. `retfq` pops the target first and
/// then the selector, so the selector must be pushed *first*.
///
/// # Safety
///
/// `selector` must name a present, 64-bit code descriptor in the GDT that was
/// loaded immediately before this call, and interrupts must be disabled.
#[inline]
pub unsafe fn reload_code_segment(selector: u16) {
    if !ON_BARE_METAL {
        return;
    }
    // SAFETY: the caller guarantees the selector names a valid 64-bit code
    // descriptor, and the far return resumes at the label in this same block.
    unsafe {
        asm!(
            "push {selector}",
            "lea {target}, [2f + rip]",
            "push {target}",
            "retfq",
            "2:",
            selector = in(reg) selector as u64,
            target = lateout(reg) _,
            options(preserves_flags),
        );
    }
}

/// Reload the data segment registers with `selector`.
///
/// # Why `SS` is loaded first
///
/// Loading `SS` is what the CPU uses to validate the stack. Doing it before
/// `DS`/`ES`/`FS`/`GS` means the stack is always consistent, and — because
/// loading `SS` inhibits interrupts for one instruction — the rest of the
/// sequence runs with interrupts masked by hardware rather than by a race
/// with the caller's `cli`.
///
/// # Safety
///
/// `selector` must name a present data descriptor in the GDT that is
/// currently loaded.
#[inline]
pub unsafe fn reload_data_segments(selector: u16) {
    if !ON_BARE_METAL {
        return;
    }
    // SAFETY: the caller guarantees the selector names a valid data
    // descriptor. No memory is accessed and the stack is untouched.
    //
    // # Why the selector is forced into `AX`
    //
    // The segment loads read `AX`, so the selector has to be in `AX`. The
    // obvious way to write this — passing the selector through a general
    // register operand and then loading `AX` from it — is wrong, because
    // the register the allocator picks is *not* guaranteed to be `AX`.
    //
    // When it is not, the selector is written to some other register and
    // `AX` keeps whatever value it had. `mov ax, ss` then loads `SS` with
    // that stale value — which, right after boot, is the firmware's `SS`.
    //
    // In this kernel that stale selector is `0x30`, which names the *high
    // half of the TSS* in the kernel's own GDT. It is not a data segment, so
    // the very next `iretq` — which reloads `SS` — raises a `#GP` whose
    // error code is the offending selector. The symptom is a kernel that
    // installs its GDT, prints the next line, and then dies on the first
    // interrupt, with the fault pointing at the `iretq` and no mention of
    // the segment table.
    //
    // Writing the selector into `AX` explicitly, with `ax` as the register
    // operand, removes the dependency on the allocator's choice.
    unsafe {
        asm!(
            "mov ss, ax",
            "mov ds, ax",
            "mov es, ax",
            "mov fs, ax",
            "mov gs, ax",
            inout("ax") selector => _,
            options(nomem, nostack),
        );
    }
}

/// Set the `FS` base to zero.
///
/// The firmware may leave a non-zero `FS` base behind for its own purposes;
/// the kernel does not use `FS`-relative addressing, and a stray base would
/// turn every `FS`-relative access into a wild pointer.
///
/// # Safety
///
/// The caller must not depend on an active FS-based memory model.
#[inline]
pub unsafe fn clear_fs_base() {
    if !ON_BARE_METAL {
        return;
    }
    // SAFETY: writing `IA32_FS_BASE` with zero is always defined.
    unsafe { wrmsr(MSR_FS_BASE, 0) };
}

/// Set the kernel `GS` base to zero.
///
/// The kernel uses `swapgs` to reach its own `GS` base on a syscall entry,
/// so it must control the value. The user's `GS` base is whatever the user
/// program set, and it is the one `swapgs` exchanges with this.
///
/// # Safety
///
/// The caller must not depend on an active kernel GS-based memory model.
#[inline]
pub unsafe fn clear_kernel_gs_base() {
    if !ON_BARE_METAL {
        return;
    }
    // SAFETY: writing `IA32_KERNEL_GS_BASE` with zero is always defined.
    unsafe { wrmsr(MSR_KERNEL_GS_BASE, 0) };
}

/// Return from an interrupt handler.
///
/// # Why this exists
///
/// A handler written in Rust is an ordinary function: when it returns, the
/// CPU expects to continue at the instruction after the call. But an
/// interrupt is *not* a call — the CPU pushed a frame onto the stack and the
/// handler must pop it and resume with `iretq`.
///
/// A handler that simply returns therefore resumes at the wrong place: the
/// CPU's pushed frame is still on the stack, and the "return address" the
/// function prologue pushed is the *handler's own* caller. The result is
/// that the handler runs its own epilogue against the interrupt frame and
/// returns into the middle of that frame — which is a double fault at best,
/// and in practice an immediate `#GP` or `#UD`.
///
/// So the handler must end with `iretq`, and this function is how. It pops
/// the frame the CPU pushed and resumes at the interrupted instruction.
///
/// # Safety
///
/// Must be called from an interrupt handler, on the stack the CPU pushed the
/// frame onto, and must not be called more than once per interrupt.
#[inline]
pub unsafe fn return_from_interrupt() -> ! {
    if !ON_BARE_METAL {
        // Off bare metal there is no interrupt frame to pop, and no way to
        // resume the interrupted code. Looping keeps the `!` return type
        // honest without executing a privileged instruction.
        loop {
            core::hint::spin_loop();
        }
    }
    // SAFETY: the caller is an interrupt handler, so the top of the stack is
    // the frame the CPU pushed: RIP, CS, RFLAGS, RSP and SS, which is exactly
    // what `iretq` pops.
    unsafe {
        asm!("iretq", options(noreturn, nostack));
    }
}

/// Drop to ring 3 through the `iretq` frame at `context`.
///
/// # Safety
///
/// `context` must point at a complete `iretq` frame — target, code selector,
/// flags, stack pointer and stack selector — naming mapped memory and
/// selectors from the GDT that is currently loaded.
#[inline]
pub unsafe fn enter_user_mode(context: u64) -> ! {
    if !ON_BARE_METAL {
        loop {
            // Off bare metal there is no ring 3 to drop to. Looping keeps the
            // function's `!` return type honest without executing a
            // privileged instruction that would fault the host.
            core::hint::spin_loop();
        }
    }
    // SAFETY: the caller guarantees the frame is a valid `iretq` frame in the
    // current address space.
    unsafe {
        asm!(
            "mov rsp, {context}",
            "iretq",
            context = in(reg) context,
            options(noreturn, nostack),
        );
    }
}
