//! LoongArch64 CSR access, IOCSR access, and memory-ordering instructions.
//!
//! Everything that touches hardware directly lives behind thin `asm!` blocks
//! here, so the rest of the HAL can be reviewed as ordinary safe Rust with a
//! small, auditable unsafe surface.
//!
//! # Safety policy
//!
//! The primitives in this module are `unsafe` because they are `unsafe` at
//! the ISA level: a CSR write can unmask interrupts machine-wide, an `invtlb`
//! can destroy translations for other tasks, and an IOCSR access can address
//! an arbitrary device. Callers are expected to be thin, named wrappers — one
//! per register or operation — so that the set of registers actually used is
//! greppable.
//!
//! Unlike x86-64, LoongArch assembly cannot be *assembled* on an x86 host, so
//! every `asm!` block in this crate is gated behind
//! `#[cfg(target_arch = "loongarch64")]`; the `#[cfg(not(...))]` twin returns
//! zero or does nothing. This is what lets `cargo test` run the pure-logic
//! halves of every driver on the development host.
//!
//! # CSR instruction semantics
//!
//! Three instructions move values between GPRs and CSRs (LoongArch Reference
//! Manual Vol. 2, Chapter 2):
//!
//! * `csrrd rd, csr`  — `rd ← CSR[csr]`.
//! * `csrwr rd, csr`  — swap: `CSR[csr] ← rd` and `rd ← old CSR[csr]`. This is
//!   why a "write" that must observe the old value is a single instruction.
//! * `csrxchg rd, rj, csr` — masked swap: `CSR[csr] ← (CSR[csr] & ~rj[13:0]) |
//!   (rd[13:0] & rj[13:0])`, `rd ← old CSR[csr]`. Only the low 14 bits of
//!   `rj` act as the mask, so callers pass the new value in `rd` and the
//!   selected bits in `rj`.
//!
//! `csrwr`/`csrxchg` on `CSR.TICLR`, `CSR.TCFG`, ... are privileged (PLV 0);
//! executing them at PLV ≥ 1 raises an IPE exception (EC 14).

#[cfg(target_arch = "loongarch64")]
use core::arch::asm;

/// `true` when compiled for bare metal (`loongarch64-unknown-none`), where the
/// CSR and IOCSR instructions actually execute.
///
/// On hosted targets every hardware path in this crate is compiled out, which
/// is what makes the crate host-testable at all.
#[cfg(target_os = "none")]
pub const ON_BARE_METAL: bool = true;
/// Hosted-target counterpart of [`ON_BARE_METAL`]; see its documentation.
#[cfg(not(target_os = "none"))]
pub const ON_BARE_METAL: bool = false;

// ---------------------------------------------------------------------------
// CSR numbers.
//
// From the LoongArch Reference Manual Vol. 2 (Chapter 2, "CSR address
// mapping"), cross-checked against Linux `arch/loongarch/include/asm/loongarch.h`.
// The CSR number is a 14-bit immediate encoded directly in the
// `csrrd`/`csrwr`/`csrxchg` instruction.
// ---------------------------------------------------------------------------

/// `CRMD` (0x00) — current mode: `PLV` bits 1:0, `IE` bit 2, `DA` bit 3,
/// `PG` bit 4, `DATF` bits 6:5, `DATL` bits 8:7.
pub const CSR_CRMD: u16 = 0x00;
/// `PRMD` (0x01) — state saved at exception entry: `PPLV` bits 1:0,
/// `PIE` bit 2.
pub const CSR_PRMD: u16 = 0x01;
/// `EUEN` (0x02) — FPU/LSX/LASX enable bits (not used by the kernel itself).
pub const CSR_EUEN: u16 = 0x02;
/// `MISC` (0x03) — `FLUSH`/`MESH`/`SUSPEND`/`RTC` bits (Loongson-specific).
pub const CSR_MISC: u16 = 0x03;
/// `ECFG` (0x04) — exception config: `LIE` bits 14:0 (local interrupt
/// enables, 15 lines), `VS` bits 18:16 (vector spacing exponent).
pub const CSR_ECFG: u16 = 0x04;
/// `ESTAT` (0x05) — exception status: `IS` bits 14:0 (interrupt samples),
/// `EC` bits 21:16 (exception code), `ESUBCODE` bits 31:22, `SUBF` bits 21:16.
pub const CSR_ESTAT: u16 = 0x05;
/// `ERA` (0x06) — exception return address.
pub const CSR_ERA: u16 = 0x06;
/// `BADV` (0x07) — bad virtual address of the faulting access.
pub const CSR_BADV: u16 = 0x07;
/// `BADI` (0x08) — bad instruction word (set for ADEF/BCE).
pub const CSR_BADI: u16 = 0x08;
/// `EENTRY` (0x0C) — exception entry base address.
pub const CSR_EENTRY: u16 = 0x0C;
/// `TLBIDX` (0x10) — TLB index/`PS`/`NE` used by `tlbwr`/`tlbrd`/`tlbsrch`.
pub const CSR_TLBIDX: u16 = 0x10;
/// `TLBEHI` (0x11) — TLB entry high: `VPPN` bits 47:13.
pub const CSR_TLBEHI: u16 = 0x11;
/// `TLBELO0` (0x12) — even-page EntryLo (bit layout: see [`paging`]).
pub const CSR_TLBELO0: u16 = 0x12;
/// `TLBELO1` (0x13) — odd-page EntryLo (bit layout: see [`paging`]).
pub const CSR_TLBELO1: u16 = 0x13;
/// `ASID` (0x18) — `ASID` bits 9:0, `ASIDSHIFT` bits 31:16.
pub const CSR_ASID: u16 = 0x18;
/// `PGDL` (0x19) — page-table base for the low half of the VA space.
pub const CSR_PGDL: u16 = 0x19;
/// `PGDH` (0x1A) — page-table base for the high half of the VA space.
pub const CSR_PGDH: u16 = 0x1A;
/// `PGD` (0x1B) — read-only view of `PGDL`/`PGDH` depending on `CRMD.PG`.
pub const CSR_PGD: u16 = 0x1B;
/// `PWCL` (0x1C) — page-walk config for the two lower levels (`PTBASE`,
/// `PTWIDTH`, `DIR1_BASE`, `DIR1_WIDTH`).
pub const CSR_PWCL: u16 = 0x1C;
/// `PWCH` (0x1D) — page-walk config for the two upper levels (`DIR2_BASE`,
/// `DIR2_WIDTH`, `DIR3_BASE`, `DIR3_WIDTH`, `HPTW_EN` bit 8).
pub const CSR_PWCH: u16 = 0x1D;
/// `STLBPS` (0x1E) — software TLB page size exponent (`PS` bits 5:0).
pub const CSR_STLBPS: u16 = 0x1E;
/// `RVCFG` (0x1F) — reduced-virtual-address config (unused on LA464).
pub const CSR_RVCFG: u16 = 0x1F;
/// `CPUID` (0x20) — read-only hardware id of this core (low bits).
pub const CSR_CPUID: u16 = 0x20;
/// `PRCFG1` (0x21) — read-only: `PALEN` bits 3:0, `VALEN` bits 7:4, `MMUVer`.
pub const CSR_PRCFG1: u16 = 0x21;
/// `PRCFG2` (0x22) — read-only: `TLBSIZ`/`TLBSET`/`MMLSIZ`/`MMUSIZ` bits.
pub const CSR_PRCFG2: u16 = 0x22;
/// `PRCFG3` (0x23) — read-only: `GRSize`/`GRIOSize`/`KSASize` bits.
pub const CSR_PRCFG3: u16 = 0x23;
/// `KS0` (0x30) — first of the sixteen read/write kernel-scratch CSRs
/// (`KS0`–`KS15`, numbers 0x30–0x3F). `TMID`/`TLBREXEC`-independent storage
/// used by [`crate::current_cpu_id`].
pub const CSR_KS0: u16 = 0x30;
/// `TMID` (0x40) — timer id / mailbox tag (Loongson-specific, unused here).
pub const CSR_TMID: u16 = 0x40;
/// `TCFG` (0x41) — timer config: `EN` bit 0, `PERIOD` bit 1, init value in
/// bits 63:2 (count of ticks; the hardware shifts left by 2 internally).
pub const CSR_TCFG: u16 = 0x41;
/// `TVAL` (0x42) — read-only remaining timer ticks.
pub const CSR_TVAL: u16 = 0x42;
/// `CNTC` (0x43) — the stable constant-frequency counter, also readable via
/// `rdtime.d rd, $zero` (see [`rdtime_d`]).
pub const CSR_CNTC: u16 = 0x43;
/// `TICLR` (0x44) — timer interrupt clear: write 1 to `TI` bit 0 to clear the
/// timer IRQ line at the CPU (not in `ECFG.LIE` — that only gates delivery).
pub const CSR_TICLR: u16 = 0x44;
/// `DMW0` (0x180) — first direct mapping window. `DMW1`–`DMW3` are 0x181–0x183.
pub const CSR_DMW0: u16 = 0x180;

/// The sixteen kernel-scratch CSRs, `KS0`–`KS15` (numbers 0x30–0x3F).
pub fn csr_ks(index: u16) -> u16 {
    debug_assert!(index < 16);
    CSR_KS0 + index
}

/// `DMW0`–`DMW3` (numbers 0x180–0x183).
pub fn csr_dmw(index: u16) -> u16 {
    debug_assert!(index < 4);
    CSR_DMW0 + index
}

// ---------------------------------------------------------------------------
// Raw CSR access.
//
// Each primitive exists in a bare-metal variant (real `asm!`) and a hosted
// variant (compiled out). The bare-metal gate is `target_arch` because
// LoongArch assembly cannot be assembled by an x86 toolchain at all.
// ---------------------------------------------------------------------------

/// Read `CSR[num]`.
///
/// # Safety
///
/// Reads are side-effect-free for the CSRs VelaOS uses, but the raw form is
/// still `unsafe` so that every register a caller touches is named.
#[inline]
pub unsafe fn read_csr<const NUM: u16>() -> u64 {
    #[cfg(target_arch = "loongarch64")]
    {
        let value: u64;
        // SAFETY: the caller guarantees `num` names a CSR this subsystem owns.
        // `csrrd` is a plain read; `nomem/nostack/preserves_flags` hold since
        // no memory, stack or arithmetic flags are touched.
        unsafe {
            asm!(
                "csrrd {rd}, {num}",
                rd = out(reg) value,
                num = const NUM,
                options(nomem, nostack, preserves_flags)
            );
        }
        value
    }
    #[cfg(not(target_arch = "loongarch64"))]
    {
        let _ = NUM;
        0
    }
}

/// Write `CSR[num] ← value`, returning the old value (the `csrwr` swap
/// semantics described in the module docs).
///
/// # Safety
///
/// The caller must own `num` and understand the machine-wide effect of the
/// write (interrupt masks, translation bases, TLB CSRs).
#[inline]
pub unsafe fn write_csr<const NUM: u16>(value: u64) -> u64 {
    #[cfg(target_arch = "loongarch64")]
    {
        let old: u64;
        // SAFETY: as [`read_csr`]; `csrwr` is a full-width swap.
        unsafe {
            asm!(
                "csrwr {val}, {num}",
                val = inout(reg) value => old,
                num = const NUM,
                options(nomem, nostack, preserves_flags)
            );
        }
        old
    }
    #[cfg(not(target_arch = "loongarch64"))]
    {
        let _ = NUM;
        let _ = value;
        0
    }
}

/// Masked CSR update: `CSR[num] = (CSR[num] & !mask) | (value & mask)`,
/// returning the old value (`csrxchg` semantics).
///
/// # Safety
///
/// As [`write_csr`].
#[inline]
pub unsafe fn xchg_csr<const NUM: u16>(value: u64, mask: u64) -> u64 {
    #[cfg(target_arch = "loongarch64")]
    {
        let old: u64;
        // SAFETY: `csrxchg` restricts the write to the bits selected by
        // `mask[13:0]`; the caller still has to own the CSR.
        unsafe {
            asm!(
                "csrxchg {val}, {mask}, {num}",
                val = inout(reg) value => old,
                mask = in(reg) mask,
                num = const NUM,
                options(nomem, nostack, preserves_flags)
            );
        }
        old
    }
    #[cfg(not(target_arch = "loongarch64"))]
    {
        let _ = NUM;
        let _ = value;
        let _ = mask;
        0
    }
}

// ---------------------------------------------------------------------------
// Raw IOCSR access (the Loongson "CSR-style" MMIO space at physical
// 0x1F000000-0x1FFFFFFF, reached by the `iocsr*` instructions rather than a
// load). Linux `arch/loongarch/include/asm/loongarch.h` documents the map;
// `crate::apic` and `crate::timer` name every offset they use.
// ---------------------------------------------------------------------------

/// Read one byte/word/doubleword from the IOCSR space.
///
/// # Safety
///
/// `addr` must be an IOCSR register the caller owns; device reads can have
/// side effects (e.g. clearing interrupt latches).
#[inline]
pub unsafe fn iocsr_read64(addr: u64) -> u64 {
    #[cfg(target_arch = "loongarch64")]
    {
        let value: u64;
        // SAFETY: the caller guarantees `addr` is an owned IOCSR register.
        unsafe {
            asm!(
                "iocsrrd.d {rd}, {rj}",
                rd = out(reg) value,
                rj = in(reg) addr,
                options(nomem, nostack, preserves_flags)
            );
        }
        value
    }
    #[cfg(not(target_arch = "loongarch64"))]
    {
        let _ = addr;
        0
    }
}

/// Write one doubleword to the IOCSR space.
///
/// # Safety
///
/// As [`iocsr_read64`]; device writes can reconfigure or corrupt hardware.
#[inline]
pub unsafe fn iocsr_write64(addr: u64, value: u64) {
    #[cfg(target_arch = "loongarch64")]
    {
        // SAFETY: the caller guarantees `addr` is an owned IOCSR register.
        unsafe {
            asm!(
                "iocsrwr.d {rd}, {rj}",
                rd = in(reg) value,
                rj = in(reg) addr,
                options(nomem, nostack, preserves_flags)
            );
        }
    }
    #[cfg(not(target_arch = "loongarch64"))]
    {
        let _ = addr;
        let _ = value;
    }
}

/// Write one word (32 bits) to the IOCSR space — the width the ExtIOI
/// registers use.
///
/// # Safety
///
/// As [`iocsr_write64`].
#[inline]
pub unsafe fn iocsr_write32(addr: u64, value: u32) {
    #[cfg(target_arch = "loongarch64")]
    {
        // SAFETY: the caller guarantees `addr` is an owned IOCSR register.
        unsafe {
            asm!(
                "iocsrwr.w {rd}, {rj}",
                rd = in(reg) value,
                rj = in(reg) addr,
                options(nomem, nostack, preserves_flags)
            );
        }
    }
    #[cfg(not(target_arch = "loongarch64"))]
    {
        let _ = addr;
        let _ = value;
    }
}

// ---------------------------------------------------------------------------
// Named accessors. One wrapper per register the kernel actually touches.
// ---------------------------------------------------------------------------

/// `CSR.CPUID` — the read-only hardware id of this core.
///
/// The scheduler works with dense *indices* rather than hardware ids; the
/// mapping is installed by [`crate::current_cpu_id::set`].
#[inline]
pub fn cpuid() -> u32 {
    // SAFETY: `CPUID` is a read-only, side-effect-free CSR.
    (unsafe { read_csr::<{ CSR_CPUID }>() } & 0xFF) as u32
}

/// `CPUCFG4` — the read-only constant-counter frequency, in ticks per second.
///
/// Kept here because it is the only `CPUCFG` the kernel reads; see
/// [`crate::timer::ticks_per_second`].
#[inline]
pub fn cpucfg4_counter_frequency() -> u32 {
    // SAFETY: `cpucfg` is a read-only, side-effect-free instruction.
    #[cfg(target_arch = "loongarch64")]
    {
        let value: u32;
        // SAFETY: `cpucfg` is a read-only architectural probe.
        unsafe {
            asm!(
                "cpucfg {rd}, {rj}",
                rd = out(reg) value,
                rj = in(reg) 4u32,
                options(nomem, nostack, preserves_flags)
            );
        }
        value
    }
    #[cfg(not(target_arch = "loongarch64"))]
    {
        // QEMU's la464 model reports 100 MHz; the hosted fallback matches it.
        crate::timer::DEFAULT_TIMER_FREQUENCY_HZ as u32
    }
}

/// `CSR.CRMD.IE` — is the global interrupt enable set?
#[inline]
pub fn crmd_ie() -> bool {
    // SAFETY: read-only CSR.
    unsafe { read_csr::<{ CSR_CRMD }>() & 0b100 != 0 }
}

/// Set or clear `CSR.CRMD.IE` (global interrupt enable, bit 2).
///
/// # Safety
///
/// Enabling interrupts can surface any pending exception or interrupt in code
/// that assumed none would fire; the caller must hold no locks that are
/// interrupt-unsafe. Disabling is always safe to call, but must be undone
/// symmetrically.
#[inline]
pub unsafe fn crmd_set_ie(enabled: bool) {
    // CRMD.IE is bit 2 — the only bit this wrapper is allowed to touch.
    let value = u64::from(enabled) << 2;
    let mask = 0b100;
    // SAFETY: the caller is responsible for the enable-interrupts hazard; the
    // masked swap cannot corrupt any other CRMD field.
    unsafe { xchg_csr::<{ CSR_CRMD }>(value, mask) };
}

/// `CSR.ECFG.LIE` — the 15-bit local interrupt-enable mask (SWI0-1 at bits
/// 0-1, HWI0-7 at bits 2-9, PCOV at 10, timer at 11, IPI at 12; bits 13-14
/// are the NMI/AVEC lines, which VelaOS leaves masked).
///
/// # Safety
///
/// Masking interrupts on/off changes which lines can preempt the caller.
#[inline]
pub unsafe fn ecfg_set_lie(lie: u16) {
    // ECFG.LIE occupies bits 14:0.
    let mask = 0x7FFF;
    // SAFETY: the caller owns the interrupt mask; other ECFG fields are
    // preserved by the masked swap.
    unsafe { xchg_csr::<{ CSR_ECFG }>(u64::from(lie) & 0x7FFF, mask) };
}

/// Read `CSR.ECFG` in full (the exception vector-spacing field `VS` lives at
/// bits 18:16 and is normally written once at boot).
#[inline]
pub fn ecfg() -> u64 {
    // SAFETY: read-only CSR.
    unsafe { read_csr::<{ CSR_ECFG }>() }
}

/// `CSR.ESTAT` — the raw exception status word.
#[inline]
pub fn estat() -> u64 {
    // SAFETY: read-only CSR.
    unsafe { read_csr::<{ CSR_ESTAT }>() }
}

/// `CSR.ERA` — the interrupted program counter.
#[inline]
pub fn era() -> u64 {
    // SAFETY: read-only CSR.
    unsafe { read_csr::<{ CSR_ERA }>() }
}

/// `CSR.BADV` — the faulting virtual address.
#[inline]
pub fn badv() -> u64 {
    // SAFETY: read-only CSR.
    unsafe { read_csr::<{ CSR_BADV }>() }
}

/// `CSR.PRMD` — the saved privilege level and interrupt enable from the
/// interrupted context, used by `ertn` to return to user mode.
#[inline]
pub fn prmd() -> u64 {
    // SAFETY: read-only CSR.
    unsafe { read_csr::<{ CSR_PRMD }>() }
}

/// `CSR.TICLR.TI ← 1` — acknowledge (clear) the timer interrupt at the CPU.
///
/// The timer line stays asserted in `ECFG.LIE` terms until this write; see
/// [`crate::timer`].
///
/// # Safety
///
/// Clears the timer interrupt for this CPU; called from the timer handler.
#[inline]
pub unsafe fn ticlr_clear_timer_irq() {
    // TICLR.TI is bit 0 and write-1-to-clear.
    // SAFETY: the only effect is clearing the timer IRQ line, which is the
    // timer handler's own job.
    unsafe { write_csr::<{ CSR_TICLR }>(1) };
}

// ---------------------------------------------------------------------------
// TLB maintenance. Op numbers from the LoongArch Reference Manual Vol. 2,
// cross-checked against QEMU `target/loongarch/tcg/insn_trans/
// trans_privileged.c.inc` (`trans_invtlb`).
// ---------------------------------------------------------------------------

/// The `invtlb` op numbers.
pub mod invtlb_op {
    /// Invalidate every TLB entry on this core.
    pub const ALL: u32 = 0;
    /// Invalidate every TLB entry whose `G` bit equals GPR[`rj`] (ops 2/3).
    pub const ALL_BY_GLOBAL: u32 = 2;
    /// Invalidate every non-global TLB entry whose ASID matches GPR[`rj`].
    pub const ALL_BY_ASID: u32 = 4;
    /// Invalidate the non-global entry matching ASID GPR[`rj`] and virtual
    /// page GPR[`rk`].
    pub const PAGE_BY_ASID: u32 = 5;
    /// As [`PAGE_BY_ASID`], but global entries at that VA are invalidated too.
    pub const PAGE_BY_ASID_OR_GLOBAL: u32 = 6;
}

/// Issue `invtlb op, rj, rk`.
///
/// # Safety
///
/// Destroys translations; the caller must have flushed any CPU-cached state
/// that depended on them (and must run with `CRMD.PG` set, since `invtlb` is
/// meaningless — and traps — under direct addressing).
#[inline]
pub unsafe fn invtlb(op: u32, rj: u64, rk: u64) {
    #[cfg(target_arch = "loongarch64")]
    {
        // SAFETY: the caller owns TLB contents and has ordered the flush.
        unsafe {
            asm!(
                "invtlb {op}, {rj}, {rk}",
                op = in(reg) op,
                rj = in(reg) rj,
                rk = in(reg) rk,
                options(nomem, nostack, preserves_flags)
            );
        }
    }
    #[cfg(not(target_arch = "loongarch64"))]
    {
        let _ = op;
        let _ = rj;
        let _ = rk;
    }
}

/// Flush the whole local TLB (`invtlb 0, $zero, $zero`).
///
/// # Safety
///
/// As [`invtlb`].
#[inline]
pub unsafe fn invtlb_flush_all() {
    // SAFETY: full flush requested by the caller.
    unsafe { invtlb(invtlb_op::ALL, 0, 0) };
}

/// Flush the local TLB of all non-global entries with the given ASID.
///
/// # Safety
///
/// As [`invtlb`].
#[inline]
pub unsafe fn invtlb_flush_asid(asid: u64) {
    // SAFETY: caller owns the ASID's translations.
    unsafe { invtlb(invtlb_op::ALL_BY_ASID, asid, 0) };
}

/// Flush the local TLB of the non-global (or global) entry covering `va` for
/// the given ASID.
///
/// # Safety
///
/// As [`invtlb`].
#[inline]
pub unsafe fn invtlb_flush_page(asid: u64, va: u64) {
    // SAFETY: caller owns the page's translations.
    unsafe { invtlb(invtlb_op::PAGE_BY_ASID_OR_GLOBAL, asid, va) };
}

// ---------------------------------------------------------------------------
// Time and ordering.
// ---------------------------------------------------------------------------

/// `rdtime.d rd, $zero` — read the stable constant-frequency counter
/// (`CSR.CNTC`).
///
/// `rj == 0` selects the stable counter; `rj == 1` would read the constant
/// *frequency*, which on LA464 is instead reported by `CPUCFG4` (see
/// [`cpucfg4_counter_frequency`]). QEMU's la464 model ticks it at 100 MHz.
#[inline]
pub fn rdtime_d() -> u64 {
    #[cfg(target_arch = "loongarch64")]
    {
        // SAFETY: a read-only architectural counter; no memory or stack use.
        let value: u64;
        unsafe {
            asm!(
                "rdtime.d {rd}, $zero",
                rd = out(reg) value,
                options(nomem, nostack, preserves_flags)
            );
        }
        value
    }
    #[cfg(not(target_arch = "loongarch64"))]
    {
        0
    }
}

/// `idle 0` — park this core until an unmasked interrupt arrives.
///
/// The operand is a 13-bit immediate; Linux and QEMU both use `idle 0`. The
/// instruction is privileged (PLV 0), and the PC is advanced past it before
/// the core halts, so the wake-up resumes after the `idle`.
///
/// # Safety
///
/// Blocks the core; only meaningful from a context that owns the CPU.
#[inline]
pub unsafe fn idle() {
    #[cfg(target_arch = "loongarch64")]
    {
        // SAFETY: `idle` is the kernel's sanctioned park-until-interrupt.
        unsafe {
            asm!("idle 0", options(nomem, nostack, preserves_flags));
        }
    }
    #[cfg(not(target_arch = "loongarch64"))]
    {}
}

/// A cheap no-op for spin loops (`dbar 0` on target — a full memory barrier
/// that also prevents the loop from being collapsed).
#[inline]
pub fn pause_hint() {
    #[cfg(target_arch = "loongarch64")]
    {
        // SAFETY: `dbar 0` orders memory and does not fault.
        unsafe {
            asm!("dbar 0", options(nomem, nostack, preserves_flags));
        }
    }
}

/// `dbar 0` — full memory barrier, used before and after TLB/CSR changes that
/// other cores observe through memory.
///
/// # Safety
///
/// Pure ordering; cannot fault.
#[inline]
pub unsafe fn dbar() {
    #[cfg(target_arch = "loongarch64")]
    {
        // SAFETY: a barrier has no architectural side effects.
        unsafe {
            asm!("dbar 0", options(nomem, nostack, preserves_flags));
        }
    }
    #[cfg(not(target_arch = "loongarch64"))]
    {}
}

/// `ibar 0` — instruction barrier, required after patching code (and after
/// `invtlb` before re-executing the affected pages).
///
/// # Safety
///
/// Pure ordering; cannot fault.
#[inline]
pub unsafe fn ibar() {
    #[cfg(target_arch = "loongarch64")]
    {
        // SAFETY: a barrier has no architectural side effects.
        unsafe {
            asm!("ibar 0", options(nomem, nostack, preserves_flags));
        }
    }
    #[cfg(not(target_arch = "loongarch64"))]
    {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn csr_numbers_match_linux() {
        // The CSR number immediates are load-bearing: a wrong number here
        // silently programs the wrong register on real hardware. These are
        // the values Linux `arch/loongarch/include/asm/loongarch.h` defines.
        assert_eq!(CSR_CRMD, 0x00);
        assert_eq!(CSR_PRMD, 0x01);
        assert_eq!(CSR_ECFG, 0x04);
        assert_eq!(CSR_ESTAT, 0x05);
        assert_eq!(CSR_ERA, 0x06);
        assert_eq!(CSR_BADV, 0x07);
        assert_eq!(CSR_EENTRY, 0x0C);
        assert_eq!(CSR_TLBIDX, 0x10);
        assert_eq!(CSR_TLBELO0, 0x12);
        assert_eq!(CSR_TLBELO1, 0x13);
        assert_eq!(CSR_ASID, 0x18);
        assert_eq!(CSR_PGDL, 0x19);
        assert_eq!(CSR_PGDH, 0x1A);
        assert_eq!(CSR_CPUID, 0x20);
        assert_eq!(CSR_TCFG, 0x41);
        assert_eq!(CSR_TVAL, 0x42);
        assert_eq!(CSR_CNTC, 0x43);
        assert_eq!(CSR_TICLR, 0x44);
        assert_eq!(CSR_DMW0, 0x180);
    }

    #[test]
    fn csr_helpers_index_correctly() {
        assert_eq!(csr_ks(0), 0x30);
        assert_eq!(csr_ks(15), 0x3F);
        assert_eq!(csr_dmw(0), 0x180);
        assert_eq!(csr_dmw(3), 0x183);
    }

    #[test]
    fn invtlb_ops_match_qemu() {
        // QEMU `trans_invtlb` dispatches on exactly these immediates.
        assert_eq!(invtlb_op::ALL, 0);
        assert_eq!(invtlb_op::ALL_BY_GLOBAL, 2);
        assert_eq!(invtlb_op::ALL_BY_ASID, 4);
        assert_eq!(invtlb_op::PAGE_BY_ASID, 5);
        assert_eq!(invtlb_op::PAGE_BY_ASID_OR_GLOBAL, 6);
    }

    #[test]
    fn hosted_hardware_reads_are_zero() {
        // Off bare metal every read compiles to a stub; the stubs must agree
        // so hosted code paths behave deterministically.
        if !ON_BARE_METAL {
            assert_eq!(cpuid(), 0);
            assert_eq!(estat(), 0);
            assert_eq!(era(), 0);
            assert_eq!(badv(), 0);
            assert_eq!(prmd(), 0);
            assert_eq!(rdtime_d(), 0);
            assert!(!crmd_ie());
        }
    }
}
