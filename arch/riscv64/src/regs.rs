//! Supervisor-mode register access: CSRs, fences, WFI, and the SBI call.
//!
//! # What S-mode may touch
//!
//! RISC-V's privilege levels gate the CSR space sharply, and the boundaries
//! are not negotiable from S-mode:
//!
//! * `sstatus`, `sie`, `sip`, `stvec`, `sepc`, `scause`, `stval`, `sscratch`
//!   and `satp` are the supervisor's own registers — fully accessible here
//!   (privileged spec, Vol. II "Supervisor CSRs").
//! * `time` (CSR 0x0C01) is readable in S-mode, provided M-mode has delegated
//!   it via `mcounteren.TM`; QEMU's firmware does. See [`crate::timer`].
//! * `mhartid`, `mstatus`, `mepc` and friends are M-mode-only: an S-mode
//!   access raises an illegal-instruction exception. Anything M-mode knows
//!   must therefore arrive through the SBI (a firmware `ecall`) or through
//!   the boot protocol — see [`hart_id`] for the classic example.
//!
//! # The SBI call convention
//!
//! An SBI call is an `ecall` from S-mode with the extension ID in `a7`, the
//! function ID in `a6`, arguments in `a0`–`a5`, and the error and value
//! returned in `a0` and `a1` (SBI spec, "SBI Call", and the legacy chapter,
//! where the extension ID *is* the function). The firmware preserves every
//! other register. [`sbi_call`] wraps exactly that contract.

use core::sync::atomic::{AtomicU64, Ordering};

/// True when this build runs on bare metal with real CSRs.
///
/// Like the x86-64 HAL, `no_std` alone does not answer the question — the
/// target triple does. `riscv64gc-unknown-none-elf` reports `target_os =
/// "none"`; a hosted build runs the unit tests and must not execute a single
/// privileged instruction.
/// True when this build runs on bare metal.
///
/// Every privileged instruction in this crate is behind this flag *and*
/// behind a target check: the flag says whether there is a machine to run
/// them on, the check says whether the compiler knows the instructions.
#[cfg(target_os = "none")]
pub const ON_BARE_METAL: bool = true;
/// True when this build runs on bare metal — false on a host build.
#[cfg(not(target_os = "none"))]
pub const ON_BARE_METAL: bool = false;

/// `sstatus.SIE` (bit 1): supervisor interrupts enabled. The hardware clears
/// it on every trap entry and `sret` restores it from `SPIE`.
pub const SSTATUS_SIE: u64 = 1 << 1;
/// `sstatus.SPIE` (bit 5): the value `SIE` takes after the next `sret`.
pub const SSTATUS_SPIE: u64 = 1 << 5;
/// `sstatus.SPP` (bit 8): the privilege level to `sret` into — 0 for U-mode,
/// 1 for S-mode. There is no separate "U bit": this one field chooses.
pub const SSTATUS_SPP: u64 = 1 << 8;
/// `sstatus.SUM` (bit 18): permit S-mode loads and stores to user pages.
///
/// Without it, the kernel cannot read user memory through its own tables —
/// an access raises a store/load page fault even though the mapping is valid.
/// Kernel code that inspects user memory must set it for the duration and
/// clear it afterwards.
pub const SSTATUS_SUM: u64 = 1 << 18;
/// `sstatus.MXR` (bit 19): make pages executable-readable, so the kernel can
/// read a user *code* page through its mapping.
pub const SSTATUS_MXR: u64 = 1 << 19;
/// The `sstatus.UXL` field (bits 33:32), shifted into position.
///
/// Encoded like `misa.MXL`: 1 = 32-bit, 2 = 64-bit. On RV64 a user mode that
/// runs 64-bit code requires `UXL = 2`; leaving it zero is not a valid
/// "whatever" — it reports U-mode XLEN as unimplemented/32-bit.
pub const SSTATUS_UXL_64: u64 = 2 << 32;

/// `sie.SSIE` (bit 1): supervisor software interrupts enabled.
pub const SIE_SSIE: u64 = 1 << 1;
/// `sie.STIE` (bit 5): supervisor timer interrupts enabled.
pub const SIE_STIE: u64 = 1 << 5;
/// `sie.SEIE` (bit 9): supervisor external interrupts enabled — the bit that
/// lets the PLIC deliver to this hart.
pub const SIE_SEIE: u64 = 1 << 9;

/// The hart id the firmware handed the kernel at boot.
///
/// `mhartid` is M-mode-only, so S-mode cannot read it. There is also no SBI
/// function that exposes it (the SBI base extension reports spec versions,
/// vendor and architecture IDs — not hart ids). The value arrives through the
/// *boot protocol* instead: OpenSBI jumps into S-mode code with the booting
/// hart's id in `a0` (and the device tree in `a1`). The kernel's entry stores
/// it here once per hart, before anything else reads it.
static BOOT_HARTID: AtomicU64 = AtomicU64::new(0);

/// Record the booting hart's id, as passed in `a0` by the firmware.
///
/// # Safety
///
/// Must be called exactly once per hart, from the kernel entry path, before
/// any code on that hart calls [`hart_id`].
pub unsafe fn set_boot_hartid(hart_id: u64) {
    BOOT_HARTID.store(hart_id, Ordering::Release);
}

/// The id of the hart executing this code, as delivered by the boot protocol.
///
/// See [`set_boot_hartid`] for why this is a cached value and not an
/// `mhartid` read: the register simply is not reachable from S-mode.
pub fn hart_id() -> u64 {
    BOOT_HARTID.load(Ordering::Acquire)
}

/// Read `sstatus`.
///
/// Returns 0 off bare metal, where no CSR can be read.
pub fn read_sstatus() -> u64 {
    if !ON_BARE_METAL {
        return 0;
    }
    // The value is declared for the target only: on a host build the
    // instruction is compiled out, and the function answers zero.
    #[cfg(target_arch = "riscv64")]
    let mut value: u64 = 0;
    #[cfg(not(target_arch = "riscv64"))]
    let value: u64 = 0;
    // SAFETY: reading `sstatus` has no side effects.
    #[cfg(target_arch = "riscv64")]
    unsafe {
        core::arch::asm!("csrr {0}, sstatus", out(reg) value);
    }
    value
}

/// True when supervisor interrupts are enabled on this hart.
pub fn interrupts_enabled() -> bool {
    if !ON_BARE_METAL {
        return false;
    }
    read_sstatus() & SSTATUS_SIE != 0
}

/// Set `sstatus.SIE`, enabling supervisor interrupts.
///
/// # Safety
///
/// The usual interrupt-hazard contract: the caller must not be holding a lock
/// the enabled interrupt's handler would take.
pub unsafe fn sstatus_ie_mask() {
    if !ON_BARE_METAL {
        #[cfg(not(target_arch = "riscv64"))]
        return;
    }
    // SAFETY: setting a single bit in `sstatus`; the caller owns the hazard.
    #[cfg(target_arch = "riscv64")]
    unsafe {
        core::arch::asm!("csrs sstatus, {0}", in(reg) SSTATUS_SIE);
    }
}

/// Clear `sstatus.SIE`, disabling supervisor interrupts.
///
/// # Safety
///
/// The caller must not rely on an interrupt arriving between the read that
/// decided to disable interrupts and this call: on a hart with a pending
/// interrupt, the handler may run after this returns.
pub unsafe fn sstatus_ie_unmask() {
    if !ON_BARE_METAL {
        #[cfg(not(target_arch = "riscv64"))]
        return;
    }
    // SAFETY: clearing a single bit in `sstatus` cannot fault.
    #[cfg(target_arch = "riscv64")]
    unsafe {
        core::arch::asm!("csrc sstatus, {0}", in(reg) SSTATUS_SIE);
    }
}

/// Halt this hart until an interrupt (or a `mip` write by M-mode) wakes it.
///
/// WFI is *not* a full stop: the hart can also wake spuriously, so a caller
/// that wants to stay asleep must loop around it. See [`crate::smp`].
pub fn wfi() {
    if !ON_BARE_METAL {
        return;
    }
    #[cfg(not(target_arch = "riscv64"))]
    {}
    // SAFETY: `wfi` is a hint; it cannot fault and has no visible side
    // effects beyond waiting.
    #[cfg(target_arch = "riscv64")]
    unsafe {
        core::arch::asm!("wfi", options(nomem, nostack, preserves_flags));
    }
}

/// A memory fence: order every prior load/store after every later one.
///
/// Used after writing page-table memory before the entry becomes reachable
/// through `satp`, the same role `mfence` plays on x86-64.
pub fn fence_rw() {
    if !ON_BARE_METAL {
        return;
    }
    #[cfg(not(target_arch = "riscv64"))]
    {}
    // SAFETY: `fence` cannot fault. `nomem` is deliberately *not* passed:
    // telling the compiler the block touches no memory would let it reorder
    // stores across the fence, which is exactly what the fence exists to
    // prevent.
    #[cfg(target_arch = "riscv64")]
    unsafe {
        core::arch::asm!("fence rw, rw", options(nostack, preserves_flags));
    }
}

/// Synchronize the instruction stream with prior writes: after modifying
/// memory the hart is about to execute, `fence.i` is mandatory.
pub fn fence_i() {
    if !ON_BARE_METAL {
        return;
    }
    #[cfg(not(target_arch = "riscv64"))]
    {}
    // SAFETY: `fence.i` cannot fault.
    #[cfg(target_arch = "riscv64")]
    unsafe {
        core::arch::asm!("fence.i", options(nomem, nostack, preserves_flags));
    }
}

/// Flush all translation caching for the current hart.
///
/// Mandatory after writing `satp` (the privileged spec requires a fence
/// before *and* after a `satp` change) and after unmapping or re-encoding a
/// PTE. VelaOS v0.1 uses only the full-flush form; `sfence.vma` with an
/// address/ASID operand narrows the flush once per-ASID bookkeeping arrives.
///
/// A no-op off bare metal, like every privileged access here: the host test
/// runner would fault on the real instruction.
///
/// # Safety
///
/// The caller must not rely on the flush having happened when this returns
/// on a hart whose TLB entries are being replaced.
pub unsafe fn sfence_vma_all() {
    if !ON_BARE_METAL {
        return;
    }
    #[cfg(not(target_arch = "riscv64"))]
    {}
    // SAFETY: a full `sfence.vma` is always well-defined in S-mode.
    #[cfg(target_arch = "riscv64")]
    unsafe {
        core::arch::asm!(
            "sfence.vma zero, zero",
            options(nomem, nostack, preserves_flags)
        );
    }
}

/// Write the `satp` register, installing an address space on this hart.
///
/// The fence before the write orders any pending table stores; the fence
/// after it orders the new `satp` against subsequent instruction fetch.
///
/// # Safety
///
/// `value` must name a live root table, and the caller must not be
/// concurrently switching address spaces on this hart.
pub unsafe fn write_satp(value: u64) {
    if !ON_BARE_METAL {
        return;
    }
    #[cfg(not(target_arch = "riscv64"))]
    let _ = value;
    // SAFETY: the caller guarantees `value` names a live root table.
    #[cfg(target_arch = "riscv64")]
    unsafe {
        core::arch::asm!(
            "fence rw, rw",
            "csrw satp, {0}",
            "sfence.vma zero, zero",
            in(reg) value,
            options(nostack, preserves_flags)
        );
    }
}

/// Issue an SBI call: extension `ext`, function `fid`, arguments `arg0`/`arg1`.
///
/// Returns `(error, value)` — `a0` and `a1` after the `ecall`, in the SBI
/// spec's order. For the legacy extensions (EIDs 0x0000–0x0008) only `a0` is
/// meaningful and it carries the *value*, not an error code; the wrappers in
/// [`crate::timer`] and [`crate::serial`] document their own shapes.
///
/// Off bare metal this returns `(0, 0)` and never reaches the firmware —
/// every caller is gated by [`ON_BARE_METAL`] anyway.
pub fn sbi_call(ext: u64, fid: u64, arg0: u64, arg1: u64) -> (u64, u64) {
    if !ON_BARE_METAL {
        return (0, 0);
    }
    // The results are declared for the target only: on a host build the
    // `ecall` is compiled out and the call answers "(0, 0)".
    #[cfg(target_arch = "riscv64")]
    let mut error: u64 = 0;
    #[cfg(target_arch = "riscv64")]
    let mut value: u64 = 0;
    #[cfg(not(target_arch = "riscv64"))]
    let (error, value) = (0u64, 0u64);
    // SAFETY: an `ecall` from S-mode is always well-defined; the firmware
    // preserves all registers except a0/a1 (SBI spec, "SBI Call"). `nomem`
    // is not passed: some extensions take pointers to memory.
    #[cfg(target_arch = "riscv64")]
    unsafe {
        core::arch::asm!(
            "ecall",
            inout("a0") arg0 => error,
            inout("a1") arg1 => value,
            in("a6") fid,
            in("a7") ext,
            options(nostack)
        );
    }
    #[cfg(not(target_arch = "riscv64"))]
    let _ = (ext, fid, arg0, arg1);
    (error, value)
}

#[cfg(test)]
extern crate std;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sstatus_bit_positions_are_distinct() {
        // SIE, SPIE and SPP live in different bits; a typo in one silently
        // changes the meaning of another.
        assert_eq!(SSTATUS_SIE, 0b10);
        assert_eq!(SSTATUS_SPIE, 1 << 5);
        assert_eq!(SSTATUS_SPP, 1 << 8);
        assert_eq!(SSTATUS_SUM, 1 << 18);
        assert_eq!(SSTATUS_MXR, 1 << 19);
        assert_eq!(SSTATUS_UXL_64 >> 32, 2, "UXL encoding 2 = XLEN-64");
        const {
            assert!(SSTATUS_SIE & SSTATUS_SPIE == 0);
            assert!(SSTATUS_SPIE & SSTATUS_SPP == 0);
            assert!(SSTATUS_SPP & SSTATUS_SUM == 0);
        }
    }

    #[test]
    fn sie_bits_match_the_privileged_spec() {
        assert_eq!(SIE_SSIE, 1 << 1);
        assert_eq!(SIE_STIE, 1 << 5);
        assert_eq!(SIE_SEIE, 1 << 9);
        const {
            assert!(SIE_SSIE & SIE_STIE == 0);
            assert!(SIE_STIE & SIE_SEIE == 0);
        }
    }

    #[test]
    fn a_fresh_hart_has_no_recorded_id() {
        // The unit tests run in a process that never booted through SBI, so
        // the cached boot id must be zero — this catches a constructor that
        // pre-sets it.
        assert_eq!(hart_id(), 0);
    }

    #[test]
    fn interrupts_report_disabled_off_bare_metal() {
        assert!(!interrupts_enabled());
        assert_eq!(read_sstatus(), 0);
    }

    #[test]
    fn sbi_call_is_inert_off_bare_metal() {
        // Never reaches the firmware on the host; returns the documented
        // (0, 0) rather than faulting.
        assert_eq!(sbi_call(0, 0, 0, 0), (0, 0));
    }
}
