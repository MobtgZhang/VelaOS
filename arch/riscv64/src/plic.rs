//! The PLIC: Platform-Level Interrupt Controller, S-mode's interrupt router.
//!
//! # What the PLIC is
//!
//! Every device-level interrupt line in the platform fans into the PLIC, and
//! the PLIC decides which *hart context* each one is delivered to. It is not
//! a CPU register file: it is a memory-mapped block whose layout is fixed by
//! the RISC-V PLIC specification, so this module can be register arithmetic
//! plus a handful of volatile accesses rather than a driver with state.
//!
//! # The register map (RISC-V PLIC spec)
//!
//! ```text
//! 0x0000_0000  reserved (priority 0 may not be set)
//! 0x0004*i    priority of interrupt source i        (i = 1..1023)
//! 0x0000_1000  pending bits, one u32 per 32 sources  (word i covers 32i..32i+31)
//! 0x0000_2000  per-context enable memory begins:
//!   0x0000_2000 + 0x0080 * ctx + 4 * (s/32)
//!            enable bit for source s in context ctx
//! 0x00C0_0000 + 0x1000 * ctx + 0
//!            threshold of context ctx (sources at or below are masked)
//! 0x00C0_0000 + 0x1000 * ctx + 4
//!            claim/complete register of context ctx
//! ```
//!
//! The spec's base addresses for a QEMU `virt` machine: the PLIC sits at
//! `0x0000_0C00_0000`, so [`PLIC_BASE`] is the module constant and every builder
//! below composes with it.
//!
//! # Contexts, and the one this kernel uses
//!
//! A *context* is a (hart, privilege) pair. Even-numbered contexts are
//! M-mode, odd-numbered are S-mode — context `2 * hart` is the hart's
//! machine context and `2 * hart + 1` its supervisor context, which is what
//! [`s_context`] builds. VelaOS runs in S-mode, so it programs context
//! `2 * hart + 1` only; an M-mode firmware (OpenSBI) has usually already
//! configured its own contexts and delegated the S-mode ones.
//!
//! # The claim/complete protocol
//!
//! An external interrupt does not name itself: the trap stub sees `scause`
//! interrupt 9 (supervisor external) and nothing else. The handler must
//! then *claim* — read the context's claim register, which atomically pops
//! the highest-priority pending-and-enabled source id — service it, and
//! *complete* — write the same id back — so the PLIC can re-assert a later
//! edge. Reading pending bits alone is not enough: a pending bit stays set
//! for level-triggered sources until the device itself de-asserts.
//!
//! # Testing
//!
//! Every offset builder is a pure `const fn`, so the map is verified on the
//! host against the spec's arithmetic. The volatile accessors are gated by
//! [`crate::regs::ON_BARE_METAL`] and are inert on the host, like every
//! hardware-touching function in this crate.

use crate::regs::ON_BARE_METAL;

/// The PLIC's physical base address on QEMU's `virt` machine.
pub const PLIC_BASE: u64 = 0x0000_0C00_0000;

/// The highest source id the PLIC reserves for platform use (spec: sources
/// 1..=1023 exist; 0 is hardwired to "no interrupt" and has no priority).
pub const MAX_SOURCE: u64 = 1023;

/// Physical address of source `source`'s 32-bit priority register.
///
/// Priority 0 masks the source entirely; 1 is the lowest live priority. The
/// spec reserves priority 0's register slot, so a caller wanting to disable
/// a source clears its *enable* bit instead of zeroing its priority.
pub const fn priority_pa(source: u64) -> u64 {
    PLIC_BASE + 4 * source
}

/// The word index of `source`'s pending bit in the pending memory.
pub const fn pending_word(source: u64) -> u64 {
    source / 32
}

/// The bit position of `source` inside its pending word.
pub const fn pending_bit(source: u64) -> u32 {
    1 << (source % 32)
}

/// Physical address of the pending word covering sources `32 * n .. 32n+31`.
pub const fn pending_pa(n: u64) -> u64 {
    PLIC_BASE + 0x1000 + 4 * n
}

/// The PLIC context number of hart `hart`'s S-mode context.
///
/// Even contexts are M-mode, odd are S-mode; VelaOS programs only the odd
/// ones. This is the single place the (hart → context) mapping lives, so a
/// platform whose firmware numbered contexts differently needs exactly one
/// edit.
pub const fn s_context(hart: u64) -> u64 {
    2 * hart + 1
}

/// Physical address of the enable word covering sources `32 * n ..= 32n+31`
/// for S-mode context `ctx`.
pub const fn enable_pa(ctx: u64, n: u64) -> u64 {
    PLIC_BASE + 0x2000 + 0x0080 * ctx + 4 * n
}

/// Physical address of context `ctx`'s threshold register.
///
/// A source is delivered only when its priority is *strictly greater* than
/// the context's threshold, so a threshold of 0 lets priority-1 sources
/// through and a threshold at the maximum masks everything.
pub const fn threshold_pa(ctx: u64) -> u64 {
    PLIC_BASE + 0x0020_0000 + 0x1000 * ctx
}

/// Physical address of context `ctx`'s claim (read) / complete (write)
/// register.
///
/// One register serves both directions: a read pops the id of the highest-
/// priority pending, enabled source (0 = nothing to service), and a write
/// of a previously claimed id marks it completed. The spec places complete
/// at the same address as claim.
pub const fn claim_pa(ctx: u64) -> u64 {
    PLIC_BASE + 0x0020_0000 + 0x1000 * ctx + 4
}

/// Interrupt source id of the UART on QEMU's `virt` machine.
pub const IRQ_UART: u64 = 10;
/// Interrupt source id of the RTC (goldfish RTC) on QEMU's `virt` machine.
pub const IRQ_RTC: u64 = 11;

/// Read one 32-bit PLIC register.
///
/// Off bare metal this returns 0: there is no MMIO window on the host, and
/// the accessors below are host-tested through their offset arithmetic
/// instead.
///
/// # Safety
///
/// `pa` must be a PLIC register address built by one of this module's
/// offset functions; an arbitrary physical address would alias whatever
/// lives there.
unsafe fn read32(pa: u64) -> u32 {
    if !ON_BARE_METAL {
        return 0;
    }
    // SAFETY: the caller names a PLIC register; a 32-bit aligned read of a
    // device register is always defined.
    unsafe { core::ptr::read_volatile(pa as *const u32) }
}

/// Write one 32-bit PLIC register.
///
/// A no-op off bare metal, like [`read32`].
///
/// # Safety
///
/// As in [`read32`]: `pa` must be a PLIC register address.
unsafe fn write32(pa: u64, value: u32) {
    if !ON_BARE_METAL {
        return;
    }
    // SAFETY: the caller names a PLIC register.
    unsafe { core::ptr::write_volatile(pa as *mut u32, value) }
}

/// Set the priority of source `source` (1..=`MAX_SOURCE`).
///
/// # Safety
///
/// Boot-time configuration: call before external interrupts are enabled on
/// any hart that routes this source.
pub unsafe fn set_priority(source: u64, priority: u32) {
    // SAFETY: `priority_pa` builds a valid PLIC register address.
    unsafe { write32(priority_pa(source), priority) }
}

/// Enable source `source` for the S-mode context of hart `hart`.
///
/// Idempotent for the same source; enables are per-(hart, source), so each
/// hart enables only what its own handlers will service.
///
/// # Safety
///
/// See [`set_priority`].
pub unsafe fn enable(hart: u64, source: u64) {
    let ctx = s_context(hart);
    let word = pending_word(source);
    // Read-modify-write, so the sources other harts already enabled in this
    // context's neighbouring words are untouched.
    // SAFETY: `enable_pa` builds a valid PLIC register address.
    let current = unsafe { read32(enable_pa(ctx, word)) };
    // SAFETY: as above.
    unsafe {
        write32(enable_pa(ctx, word), current | pending_bit(source));
    }
}

/// Set the acceptance threshold of hart `hart`'s S-mode context.
///
/// # Safety
///
/// See [`set_priority`].
pub unsafe fn set_threshold(hart: u64, threshold: u32) {
    // SAFETY: `threshold_pa` builds a valid PLIC register address.
    unsafe { write32(threshold_pa(s_context(hart)), threshold) }
}

/// Claim the highest-priority pending source for hart `hart`'s S-mode
/// context.
///
/// Returns the source id, or `None` when nothing is waiting. The claimed
/// id must eventually be handed to [`complete`] or the PLIC will never
/// deliver that source again.
///
/// # Safety
///
/// Only call from the external-interrupt handler of hart `hart`, with that
/// hart's context already configured.
pub unsafe fn claim(hart: u64) -> Option<u64> {
    // SAFETY: `claim_pa` builds a valid PLIC register address.
    let id = unsafe { read32(claim_pa(s_context(hart))) } as u64;
    (id != 0).then_some(id)
}

/// Mark source `source` serviced for hart `hart`'s S-mode context.
///
/// # Safety
///
/// Only for an id previously returned by [`claim`] on the same hart; the
/// PLIC treats any other value as a context error.
pub unsafe fn complete(hart: u64, source: u64) {
    // SAFETY: `claim_pa` builds a valid PLIC register address; the caller
    // guarantees the claim/complete pairing.
    unsafe { write32(claim_pa(s_context(hart)), source as u32) }
}

/// True when source `source` is pending, read from the pending memory.
///
/// Diagnostics only: the claim/complete protocol, not a pending-bit poll,
/// is what moves interrupts.
pub fn is_pending(source: u64) -> bool {
    // SAFETY: `pending_pa` builds a valid PLIC register address.
    unsafe { read32(pending_pa(pending_word(source))) & pending_bit(source) != 0 }
}

#[cfg(test)]
extern crate std;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn priority_registers_start_at_base_plus_four() {
        // Source 0 has no register (the spec reserves it), so the first
        // priority register is source 1's at base + 4.
        assert_eq!(priority_pa(1), 0x0000_0C00_0004);
        assert_eq!(priority_pa(10), 0x0000_0C00_0028);
        assert_eq!(priority_pa(MAX_SOURCE), 0x0000_0C00_0FFC);
    }

    #[test]
    fn pending_memory_sits_at_offset_0x1000() {
        assert_eq!(pending_pa(0), 0x0000_0C00_1000);
        assert_eq!(pending_pa(1), 0x0000_0C00_1004);
        // The UART (source 10) lives in word 0, bit 10.
        assert_eq!(pending_word(IRQ_UART), 0);
        assert_eq!(pending_bit(IRQ_UART), 1 << 10);
    }

    #[test]
    fn pending_bits_split_across_words_by_thirty_two() {
        // Source 33 must land in word 1, bit 1 — an off-by-one here makes
        // one source's status read as another's.
        assert_eq!(pending_word(33), 1);
        assert_eq!(pending_bit(33), 1 << 1);
        assert_eq!(pending_word(32), 1);
        assert_eq!(pending_bit(32), 1);
        assert_eq!(pending_word(31), 0);
        assert_eq!(pending_bit(31), 1 << 31);
    }

    #[test]
    fn s_contexts_are_the_odd_numbers() {
        // Hart 0's S-mode context is 1; hart 1's is 3. The even numbers are
        // the M-mode contexts OpenSBI owns.
        assert_eq!(s_context(0), 1);
        assert_eq!(s_context(1), 3);
        assert_eq!(s_context(2), 5);
        for hart in 0..8u64 {
            assert_eq!(s_context(hart) % 2, 1, "S-mode contexts are odd");
        }
    }

    #[test]
    fn enable_memory_is_eighty_bytes_per_context() {
        // One context consumes 32 enable words of 4 bytes = 128 bytes... the
        // spec actually strides 0x0080 = 128 bytes per context; the arithmetic
        // below pins both directions of that division.
        assert_eq!(enable_pa(0, 0), 0x0000_0C00_2000);
        assert_eq!(enable_pa(1, 0), 0x0000_0C00_2080);
        assert_eq!(enable_pa(2, 0), 0x0000_0C00_2100);
        // Word 1 of hart 0's S-mode context. That context is number 1 —
        // the M-mode context is 0 — so its enable block starts one stride
        // along, at 0x2080, and word 1 is four bytes into it.
        assert_eq!(enable_pa(s_context(0), 1), 0x0000_0C00_2084);
        // Hart 1's S-mode context is 3, so its enable block is three
        // strides along.
        assert_eq!(enable_pa(s_context(1), 0), 0x0000_0C00_2180);
    }

    #[test]
    fn threshold_and_claim_start_at_offset_0x0020_0000() {
        assert_eq!(threshold_pa(0), 0x0000_0C20_0000);
        assert_eq!(threshold_pa(1), 0x0000_0C20_1000);
        assert_eq!(claim_pa(0), 0x0000_0C20_0004);
        assert_eq!(claim_pa(1), 0x0000_0C20_1004);
        // Threshold and claim of the same context sit 4 bytes apart — one
        // is read-only-ish, the other write-only-ish, but they are separate
        // registers, not one field.
        assert_eq!(claim_pa(1) - threshold_pa(1), 4);
    }

    #[test]
    fn the_context_stride_is_one_kib() {
        // The claim/complete block strides 0x1000 per context; this is the
        // value every builder shares, so pin it once.
        assert_eq!(threshold_pa(2) - threshold_pa(1), 0x1000);
        assert_eq!(claim_pa(2) - claim_pa(1), 0x1000);
    }

    #[test]
    fn qemu_virt_sources_are_the_documented_ids() {
        assert_eq!(IRQ_UART, 10);
        assert_eq!(IRQ_RTC, 11);
    }

    #[test]
    fn accessors_are_inert_off_bare_metal() {
        // The whole volatile surface must be safe to call on the host: it
        // returns defaults instead of touching an MMIO window.
        assert!(!is_pending(IRQ_UART));
        // SAFETY: these are the documented no-ops off bare metal.
        unsafe {
            set_priority(IRQ_UART, 1);
            enable(0, IRQ_UART);
            set_threshold(0, 0);
            assert_eq!(claim(0), None);
            complete(0, IRQ_UART);
        }
    }
}
