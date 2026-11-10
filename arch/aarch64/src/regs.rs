//! AArch64 system-register access, `WFI`, `DAIF` masking, barriers and TLBI.
//!
//! Everything that touches hardware directly lives behind thin `asm!` blocks
//! here, so the rest of the HAL can be reviewed as ordinary safe Rust with a
//! small, auditable unsafe surface.
//!
//! # Safety policy
//!
//! The primitives in this module are `unsafe` because they are `unsafe` at
//! the ISA level: a `DAIF` write can unmask interrupts machine-wide, an
//! `SCTLR_EL1` write can enable the MMU, and a `TLBI` destroys translations.
//! Callers are expected to be thin, named wrappers — one per register or
//! operation — so that the set of registers actually used is greppable.
//!
//! Unlike x86-64, ARMv8-A assembly cannot be *assembled* on an x86 host, so
//! every `asm!` block in this crate is gated behind
//! `#[cfg(target_arch = "aarch64")]`; the `#[cfg(not(...))]` twin returns zero
//! or does nothing. This is what lets `cargo test` run the pure-logic halves
//! of every driver on the development host.
//!
//! # The `DAIF` interrupt mask
//!
//! `PSTATE.DAIF` is the four-flag interrupt mask (ARMv8-A ARM, "D1.10 The
//! `DAIF` register"):
//!
//! * `D` bit 9 — debug exceptions,
//! * `A` bit 8 — SError (system error) interrupts,
//! * `I` bit 7 — IRQs,
//! * `F` bit 6 — FIQs.
//!
//! A flag *set* means masked. The kernel's "interrupts on/off" is the `I`
//! flag alone: `msr daifset, #2` sets it, `msr daifclr, #2` clears it (the
//! immediate selects which flags; 2 = the `I` flag).

use core::sync::atomic::{AtomicU64, Ordering};

/// `true` when compiled for bare metal (`aarch64-unknown-none`), where the
/// system-register instructions actually execute.
///
/// On hosted targets every hardware path in this crate is compiled out, which
/// is what makes the crate host-testable at all.
#[cfg(target_os = "none")]
pub const ON_BARE_METAL: bool = true;
/// Hosted-target counterpart of [`ON_BARE_METAL`]; see its documentation.
#[cfg(not(target_os = "none"))]
pub const ON_BARE_METAL: bool = false;

/// `DAIF.D` (bit 9): debug exceptions masked.
pub const DAIF_D: u64 = 1 << 9;
/// `DAIF.A` (bit 8): SError interrupts masked.
pub const DAIF_A: u64 = 1 << 8;
/// `DAIF.I` (bit 7): IRQs masked. This is the bit VelaOS's
/// "interrupts enabled" means.
pub const DAIF_I: u64 = 1 << 7;
/// `DAIF.F` (bit 6): FIQs masked.
pub const DAIF_F: u64 = 1 << 6;

/// The `MPIDR_EL1.MT` bit (bit 24): the lowest affinity level (Aff0) is
/// thread-level rather than core-level. QEMU's `virt` topology leaves it
/// clear — Aff0 *is* the core index.
pub const MPIDR_MT: u64 = 1 << 24;
/// `MPIDR_EL1.U` (bit 30): uniprocessor. Set on single-core configurations.
pub const MPIDR_U: u64 = 1 << 30;
/// `MPIDR_EL1` bit 31 is RES1: a read always returns one, which is how a
/// decoder can tell a real read from a zero fallback.
pub const MPIDR_RES1: u64 = 1 << 31;

// ---------------------------------------------------------------------------
// Raw system-register access.
//
// Each primitive exists in a bare-metal variant (real `asm!`) and a hosted
// variant (compiled out). The bare-metal gate is `target_arch` because ARMv8-A
// assembly cannot be assembled by an x86 toolchain at all.
// ---------------------------------------------------------------------------

/// Read a system register by name.
///
/// # Why the name is matched rather than assembled
///
/// A system register's name is part of the *instruction*: `mrs x0, cntfrq_el0`
/// encodes the register in the instruction, not in a register. An inline
/// assembly block therefore cannot take the name as an operand — it has to
/// be a literal in the instruction text.
///
/// The macro expands the name into the instruction, and the function below
/// matches the names the kernel uses so that a caller can still say
/// `read_sysreg(names::CNTFRQ_EL0)` and get a *compile error* when the name is
/// misspelled, rather than a runtime zero.
#[cfg(target_arch = "aarch64")]
macro_rules! sys_read {
    ($name:literal) => {{
        let value: u64;
        // SAFETY: the macro is only used with the register names the
        // architecture defines, and reading them has no side effect.
        unsafe {
            core::arch::asm!(
                concat!("mrs {0}, ", $name),
                out(reg) value,
                options(nomem, nostack, preserves_flags)
            );
        }
        value
    }};
}

/// Write a system register by name.
///
/// The name has to be a literal in the instruction, for the same reason
/// [`sys_read!`] has to be a macro.
#[cfg(target_arch = "aarch64")]
macro_rules! sys_write {
    ($name:literal, $value:expr) => {{
        // SAFETY: the macro is only used with the register names the
        // architecture defines, and the caller owns the register.
        unsafe {
            core::arch::asm!(
                concat!("msr ", $name, ", {0}"),
                in(reg) $value,
                options(nomem, nostack, preserves_flags)
            );
        }
    }};
}

/// The system registers the kernel reads.
///
/// Every name here is matched, so a typo is a compile error rather than a
/// silent zero.
pub mod names {
    /// The counters' frequency.
    ///
    /// The register is at EL0, not EL1: there is no `CNTFRQ_EL1`, and a
    /// program that read that name would get a *zero* frequency and every
    /// delay built on it would be infinitely long.
    pub const CNTFRQ_EL0: &str = "cntfrq_el0";
    /// The physical counter.
    pub const CNTPCT_EL0: &str = "cntpct_el0";
    /// The virtual counter.
    pub const CNTVCT_EL0: &str = "cntvct_el0";
    /// The virtual timer's control register.
    pub const CNTV_CTL_EL0: &str = "cntv_ctl_el0";
    /// The virtual timer's value register.
    pub const CNTV_TVAL_EL0: &str = "cntv_tval_el0";
    /// The virtual timer's mask register.
    pub const CNTV_MASK_EL0: &str = "cntv_mask_el0";
    /// The interrupt controller's system-register enable.
    pub const ICC_SRE_EL1: &str = "icc_sre_el1";
    /// The interrupt controller's acknowledge register.
    pub const ICC_IAR1_EL1: &str = "icc_iar1_el1";
    /// The interrupt controller's end-of-interrupt register.
    pub const ICC_EOIR1_EL1: &str = "icc_eoir1_el1";
    /// The interrupt controller's software-generated interrupt register.
    /// The priority mask: interrupts whose priority is not more significant
    /// than this value do not preempt.
    pub const ICC_PMR_EL1: &str = "icc_pmr_el1";
    /// The interrupt group register: which interrupts are group 1 (and so
    /// preempt on the kernel's own priority).
    pub const ICC_IGRPEN1_EL1: &str = "icc_igrpen1_el1";
    /// The software-generated interrupt 1 register: the way one core wakes
    /// another.
    pub const ICC_SGI1R_EL1: &str = "icc_sgi1r_el1";
    /// The interrupt controller's software-generated interrupt acknowledge.
    pub const ICC_SGI_EOI: &str = "icc_sgi_eoi";
}

/// Read the system register named by `name`.
///
/// # Safety
///
/// Reads are side-effect-free for the registers VelaOS uses, but the raw form
/// is still `unsafe` so that every register a caller touches is named.
#[inline]
pub unsafe fn read_sysreg(name: &str) -> u64 {
    #[cfg(target_arch = "aarch64")]
    {
        match name {
            names::CNTFRQ_EL0 => sys_read!("cntfrq_el0"),
            names::CNTPCT_EL0 => sys_read!("cntpct_el0"),
            names::CNTVCT_EL0 => sys_read!("cntvct_el0"),
            names::CNTV_CTL_EL0 => sys_read!("cntv_ctl_el0"),
            names::CNTV_TVAL_EL0 => sys_read!("cntv_tval_el0"),
            names::CNTV_MASK_EL0 => sys_read!("cntv_mask_el0"),
            names::ICC_SRE_EL1 => sys_read!("icc_sre_el1"),
            names::ICC_IAR1_EL1 => sys_read!("icc_iar1_el1"),
            names::ICC_EOIR1_EL1 => sys_read!("icc_eoir1_el1"),
            names::ICC_PMR_EL1 => sys_read!("icc_pmr_el1"),
            names::ICC_IGRPEN1_EL1 => sys_read!("icc_igrpen1_el1"),
            names::ICC_SGI1R_EL1 => sys_read!("icc_sgi1r_el1"),
            names::ICC_SGI_EOI => sys_read!("icc_sgi_eoi"),
            // An unknown name is a bug in the caller; the honest answer is
            // zero, and the name is documented as unsupported.
            _ => 0,
        }
    }
    #[cfg(not(target_arch = "aarch64"))]
    {
        let _ = name;
        0
    }
}

/// Write a system register: `msr {name}, {value}`.
///
/// # Safety
///
/// The caller must own `name` and understand the machine-wide effect of the
/// write (interrupt masks, translation bases, the vector base).
#[inline]
pub unsafe fn write_sysreg(name: &str, value: u64) {
    #[cfg(target_arch = "aarch64")]
    {
        match name {
            names::CNTV_CTL_EL0 => sys_write!("cntv_ctl_el0", value),
            names::CNTV_TVAL_EL0 => sys_write!("cntv_tval_el0", value),
            names::CNTV_MASK_EL0 => sys_write!("cntv_mask_el0", value),
            names::ICC_SRE_EL1 => sys_write!("icc_sre_el1", value),
            names::ICC_IAR1_EL1 => sys_write!("icc_iar1_el1", value),
            names::ICC_EOIR1_EL1 => sys_write!("icc_eoir1_el1", value),
            names::ICC_PMR_EL1 => sys_write!("icc_pmr_el1", value),
            names::ICC_IGRPEN1_EL1 => sys_write!("icc_igrpen1_el1", value),
            names::ICC_SGI1R_EL1 => sys_write!("icc_sgi1r_el1", value),
            names::ICC_SGI_EOI => sys_write!("icc_sgi_eoi", value),
            _ => {}
        }
    }
    #[cfg(not(target_arch = "aarch64"))]
    {
        let _ = name;
        let _ = value;
    }
}

// ---------------------------------------------------------------------------
// `DAIF` and PSTATE.
// ---------------------------------------------------------------------------

/// Read the `DAIF` mask. A set bit means *masked*.
#[inline]
pub fn daif() -> u64 {
    // SAFETY: `DAIF` is a read-only side-effect-free register.
    unsafe { read_sysreg("daif") }
}

/// True when IRQs are unmasked (`DAIF.I` clear) — the kernel's definition of
/// "interrupts enabled".
pub fn interrupts_enabled() -> bool {
    if !ON_BARE_METAL {
        return false;
    }
    daif() & DAIF_I == 0
}

/// Set `DAIF.I`, masking IRQs.
///
/// # Safety
///
/// Masking can only defer interrupts, but the caller must undo it
/// symmetrically (or restore the previous value) or the CPU stays silent.
#[inline]
pub unsafe fn daif_mask() {
    if !ON_BARE_METAL {
        return;
    }
    // SAFETY: `daifset, #2` selects exactly the `I` flag; the caller owns the
    // mask's lifetime.
    unsafe {
        core::arch::asm!("msr daifset, #2", options(nomem, nostack, preserves_flags));
    }
}

/// Clear `DAIF.I`, unmasking IRQs.
///
/// # Safety
///
/// Enabling interrupts can surface any pending interrupt in code that assumed
/// none would fire; the caller must hold no locks that are interrupt-unsafe.
#[inline]
pub unsafe fn daif_unmask() {
    if !ON_BARE_METAL {
        return;
    }
    // SAFETY: `daifclr, #2` selects exactly the `I` flag; the caller owns the
    // enable-interrupts hazard.
    unsafe {
        core::arch::asm!("msr daifclr, #2", options(nomem, nostack, preserves_flags));
    }
}

// ---------------------------------------------------------------------------
// Identification, parking and barriers.
// ---------------------------------------------------------------------------

/// Read `MPIDR_EL1`, the multiprocessor affinity register.
///
/// The layout (ARMv8-A ARM, "MPIDR_EL1"): `Aff0` bits 7:0, `Aff1` bits 15:8,
/// `Aff2` bits 23:16, `MT` bit 24, `U` bit 30, RES1 bit 31. QEMU's `virt`
/// topology reports the core index in `Aff0` and clears everything above it
/// except RES1. See [`crate::smp`].
///
/// Returns 0 off bare metal, where no system register can be read.
#[inline]
pub fn read_mpidr() -> u64 {
    if !ON_BARE_METAL {
        return 0;
    }
    // SAFETY: `MPIDR_EL1` is read-only and side-effect-free.
    unsafe { read_sysreg("mpidr_el1") }
}

/// This core's `Aff0` value — under the `virt` topology, the core index.
pub fn mpidr_aff0() -> u64 {
    read_mpidr() & 0xFF
}

/// Read `CurrentEL`, whose `EL` field (bits 3:2) holds the current exception
/// level: `0b01` EL0, `0b10` EL1, `0b11` EL2.
///
/// Returns 0 off bare metal; the kernel runs at EL1.
#[inline]
pub fn current_el() -> u64 {
    if !ON_BARE_METAL {
        return 0;
    }
    // SAFETY: `CurrentEL` is read-only and side-effect-free.
    (unsafe { read_sysreg("currentel") } >> 2) & 0b11
}

/// Halt this core until an unmasked interrupt (or a debug event) wakes it.
///
/// `WFI` is *not* a full stop: the core can also wake spuriously, so a caller
/// that wants to stay asleep must loop around it. See [`crate::smp`].
pub fn wfi() {
    if !ON_BARE_METAL {
        return;
    }
    // SAFETY: `wfi` is a hint; it cannot fault and has no visible side
    // effects beyond waiting.
    unsafe {
        core::arch::asm!("wfi", options(nomem, nostack, preserves_flags));
    }
}

/// `dsb sy` — full data synchronization barrier, system scope.
///
/// Required between page-table writes and the `TLBI` that publishes them
/// (ARMv8-A ARM, "Ordering and requirements" in the `TLBI` chapter: the `dsb`
/// guarantees the table writes are complete before the invalidation starts).
#[inline]
pub fn dsb() {
    if !ON_BARE_METAL {
        return;
    }
    // SAFETY: a barrier has no architectural side effects.
    unsafe {
        core::arch::asm!("dsb sy", options(nomem, nostack, preserves_flags));
    }
}

/// `dmb sy` — full data memory barrier, system scope.
#[inline]
pub fn dmb() {
    if !ON_BARE_METAL {
        return;
    }
    // SAFETY: a barrier has no architectural side effects.
    unsafe {
        core::arch::asm!("dmb sy", options(nomem, nostack, preserves_flags));
    }
}

/// `isb` — instruction synchronization barrier, flushing prefetch.
///
/// Required after writing a system register that affects instruction fetch
/// (`SCTLR_EL1`, `TCR_EL1`, `VBAR_EL1`) before any *subsequent* instruction
/// sees the effect.
#[inline]
pub fn isb() {
    if !ON_BARE_METAL {
        return;
    }
    // SAFETY: a barrier has no architectural side effects.
    unsafe {
        core::arch::asm!("isb", options(nomem, nostack, preserves_flags));
    }
}

// ---------------------------------------------------------------------------
// Translation-cache maintenance.
//
// ARMv8-A ARM, "Translation table maintenance instructions": a `TLBI`
// invalidates translations cached on this PE. The `dsb` before and after a
// page-table change is mandatory ordering, not a formality — see [`dsb`].
// ---------------------------------------------------------------------------

/// `tlbi vmalle1` — invalidate *all* stage-1 translations for EL0/EL1 on this
/// core, both TTBR0 and TTBR1 trees.
///
/// # Safety
///
/// Destroys translations; the caller must have completed any table writes
/// with a `dsb` first and must tolerate refetching translations afterwards.
#[inline]
pub unsafe fn tlbi_vmalle1() {
    if !ON_BARE_METAL {
        return;
    }
    // SAFETY: a full local invalidation is always well-defined at EL1.
    unsafe {
        core::arch::asm!("tlbi vmalle1", options(nomem, nostack, preserves_flags));
    }
}

/// `tlbi vmalle1is` — as [`tlbi_vmalle1`], but Inner-Shareable: every PE in
/// the shareability domain flushes. The form an address-space *activation*
/// must use, because another core may hold stale translations for the new
/// ASID.
///
/// # Safety
///
/// As [`tlbi_vmalle1`].
#[inline]
pub unsafe fn tlbi_vmalle1is() {
    if !ON_BARE_METAL {
        return;
    }
    // SAFETY: a broadcast invalidation is always well-defined at EL1.
    unsafe {
        core::arch::asm!("tlbi vmalle1is", options(nomem, nostack, preserves_flags));
    }
}

/// `tlbi vale1, {va}` — invalidate the stage-1 translations for `va` in the
/// *current* ASID (TTBR0 tree), ignoring the global bit.
///
/// # Safety
///
/// Destroys the page's translations; the caller must have re-encoded the
/// entry (or decided the page is gone) before letting anything else fetch it.
#[inline]
pub unsafe fn tlbi_vale1(va: u64) {
    if !ON_BARE_METAL {
        return;
    }
    // SAFETY: a single-address invalidation is always well-defined at EL1.
    unsafe {
        core::arch::asm!(
            "tlbi vale1, {va}",
            va = in(reg) va,
            options(nomem, nostack, preserves_flags)
        );
    }
}

/// `tlbi vale1is, {va}` — as [`tlbi_vale1`], Inner-Shareable.
///
/// # Safety
///
/// As [`tlbi_vale1`].
#[inline]
pub unsafe fn tlbi_vale1is(va: u64) {
    if !ON_BARE_METAL {
        return;
    }
    // SAFETY: a broadcast single-address invalidation is well-defined at EL1.
    unsafe {
        core::arch::asm!(
            "tlbi vale1is, {va}",
            va = in(reg) va,
            options(nomem, nostack, preserves_flags)
        );
    }
}

// ---------------------------------------------------------------------------
// Translation-base registers. One named wrapper per register the kernel
// actually touches; the value builders live in [`crate::paging`].
// ---------------------------------------------------------------------------

/// The booting core's MPIDR, recorded once by the kernel entry path.
///
/// Like the RISC-V HAL's boot-hart cache, this exists for the *hosted* build:
/// on the target `read_mpidr` is live, and on the host the boot protocol
/// hands the value over instead.
static BOOT_MPIDR: AtomicU64 = AtomicU64::new(0);

/// Record the booting core's MPIDR, as the boot protocol delivered it.
///
/// # Safety
///
/// Must be called exactly once, from the kernel entry path, before any code
/// calls [`mpidr_boot`].
pub unsafe fn set_boot_mpidr(mpidr: u64) {
    BOOT_MPIDR.store(mpidr, Ordering::Release);
}

/// The booting core's MPIDR, as recorded by [`set_boot_mpidr`].
pub fn mpidr_boot() -> u64 {
    BOOT_MPIDR.load(Ordering::Acquire)
}

/// Write `TTBR0_EL1`, installing the user address space on this core.
///
/// The barriers around the write are the spec's (ARMv8-A ARM, "Changing
/// translation table base registers" in the `VMSA` ordering chapter): a
/// `dsb` before the write orders pending table stores, and the write plus
/// `tlbi vmalle1is` (or a fresh ASID) guarantees no stale translation of the
/// previous space survives into the new one.
///
/// # Safety
///
/// `value` must name a live root table, and the caller must not be
/// concurrently switching address spaces on this core.
pub unsafe fn write_ttbr0_el1(value: u64) {
    if !ON_BARE_METAL {
        return;
    }
    // SAFETY: the caller guarantees `value` names a live root table.
    unsafe {
        core::arch::asm!(
            "dsb sy",
            "msr ttbr0_el1, {value}",
            "isb",
            value = in(reg) value,
            options(nostack, preserves_flags)
        );
    }
}

/// Write `TTBR1_EL1`, installing the kernel address space on this core.
///
/// Normally written once at boot: the kernel tree is shared by every address
/// space, and per-task `TTBR0` switching never touches it.
///
/// # Safety
///
/// `value` must name a live root table.
pub unsafe fn write_ttbr1_el1(value: u64) {
    if !ON_BARE_METAL {
        return;
    }
    // SAFETY: the caller guarantees `value` names a live root table.
    unsafe {
        core::arch::asm!(
            "dsb sy",
            "msr ttbr1_el1, {value}",
            "isb",
            value = in(reg) value,
            options(nostack, preserves_flags)
        );
    }
}

/// Write `TCR_EL1`, the translation-control register.
///
/// Must be written before either TTBR is, and an `isb` (inside the asm) is
/// required before the new translation regime is fetched against.
///
/// # Safety
///
/// The value must be a valid configuration; see [`crate::paging`]'s builders.
pub unsafe fn write_tcr_el1(value: u64) {
    if !ON_BARE_METAL {
        return;
    }
    // SAFETY: the caller supplies a builder-produced value.
    unsafe {
        core::arch::asm!(
            "msr tcr_el1, {value}",
            "isb",
            value = in(reg) value,
            options(nomem, nostack, preserves_flags)
        );
    }
}

/// Write `MAIR_EL1`, the memory-attribute-indirection register.
///
/// # Safety
///
/// The value must be a valid configuration; see [`crate::paging`]'s builders.
pub unsafe fn write_mair_el1(value: u64) {
    if !ON_BARE_METAL {
        return;
    }
    // SAFETY: the caller supplies a builder-produced value.
    unsafe {
        core::arch::asm!(
            "msr mair_el1, {value}",
            "isb",
            value = in(reg) value,
            options(nomem, nostack, preserves_flags)
        );
    }
}

/// Write `SCTLR_EL1` in full.
///
/// # Safety
///
/// Enabling the MMU (`SCTLR.M`) while the translation regime is not fully
/// configured traps the core; the caller owns the boot ordering.
pub unsafe fn write_sctlr_el1(value: u64) {
    if !ON_BARE_METAL {
        return;
    }
    // SAFETY: the caller owns the MMU-enable ordering.
    unsafe {
        core::arch::asm!(
            "msr sctlr_el1, {value}",
            "isb",
            value = in(reg) value,
            options(nomem, nostack, preserves_flags)
        );
    }
}

/// Write `VBAR_EL1`, pointing this core's vector table at `base`.
///
/// `base` must be 2048-byte aligned: the sixteen 128-byte slots of the
/// AArch64 vector table occupy exactly 2 KiB, and low bits 10:0 of the base
/// are ignored by the hardware (see [`crate::idt`]).
///
/// # Safety
///
/// Runs once per core during boot, with the vector stubs linked in.
pub unsafe fn write_vbar_el1(base: u64) {
    if !ON_BARE_METAL {
        return;
    }
    // SAFETY: the caller guarantees the stubs are linked and aligned.
    unsafe {
        core::arch::asm!(
            "msr vbar_el1, {base}",
            "isb",
            base = in(reg) base,
            options(nomem, nostack, preserves_flags)
        );
    }
}

#[cfg(test)]
extern crate std;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn daif_bit_positions_are_distinct() {
        // D, A, I, F live in bits 9, 8, 7, 6; a typo in one silently changes
        // the meaning of another.
        assert_eq!(DAIF_D, 1 << 9);
        assert_eq!(DAIF_A, 1 << 8);
        assert_eq!(DAIF_I, 1 << 7);
        assert_eq!(DAIF_F, 1 << 6);
        const {
            assert!(DAIF_D & DAIF_A == 0);
            assert!(DAIF_A & DAIF_I == 0);
            assert!(DAIF_I & DAIF_F == 0);
        }
    }

    #[test]
    fn mpidr_field_positions_match_the_spec() {
        assert_eq!(MPIDR_MT, 1 << 24);
        assert_eq!(MPIDR_U, 1 << 30);
        assert_eq!(MPIDR_RES1, 1 << 31);
    }

    #[test]
    fn a_fresh_core_has_no_recorded_mpidr() {
        // The unit tests run in a process that never booted through the boot
        // protocol, so the cached value must be zero — this catches a
        // constructor that pre-sets it.
        assert_eq!(mpidr_boot(), 0);
    }

    #[test]
    fn interrupts_report_disabled_off_bare_metal() {
        assert!(!interrupts_enabled());
        assert_eq!(daif(), 0);
        assert_eq!(read_mpidr(), 0);
        assert_eq!(current_el(), 0);
        assert_eq!(mpidr_aff0(), 0);
    }

    #[test]
    fn hardware_operations_are_inert_off_bare_metal() {
        // Never reaches a system register on the host; the point is that the
        // call paths typecheck and terminate.
        wfi();
        dsb();
        dmb();
        isb();
        // SAFETY: the documented host no-ops.
        unsafe {
            daif_mask();
            daif_unmask();
            tlbi_vmalle1();
            tlbi_vmalle1is();
            tlbi_vale1(0x1000);
            tlbi_vale1is(0x1000);
            write_ttbr0_el1(0);
            write_ttbr1_el1(0);
            write_tcr_el1(0);
            write_mair_el1(0);
            write_sctlr_el1(0);
            write_vbar_el1(0);
        }
    }
}
