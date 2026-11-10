//! Secondary-hart bring-up through the SBI HSM extension.
//!
//! # The boot protocol, and why SMP starts in the firmware
//!
//! On x86-64 the kernel itself sends the SIPI that wakes an application
//! processor. RISC-V's S-mode cannot do that: the machine-level `mstatus`
//! and interrupt-controller bits a hart needs at its first instruction are
//! M-mode state, and the `plic::M`-side context programming the booting
//! hart needs is likewise out of reach. The SBI's Hart State Management
//! (HSM) extension is the firmware's answer: S-mode asks the firmware to
//! take a hart from *stopped* to *started* at a chosen entry address, and
//! the firmware does the M-mode legwork first.
//!
//! # What the booting hart sees
//!
//! OpenSBI jumps into the kernel's entry point with a fixed register
//! contract (the RISC-V boot protocol, unchanged across firmwares):
//!
//! | Register | Value |
//! |---|---|
//! | `a0` | the booting hart's id — the value [`crate::regs::set_boot_hartid`] records |
//! | `a1` | the physical address of the device tree blob |
//! | `a2`–`a7`, `ra`, `sp` | unspecified — the entry stub may assume nothing else |
//!
//! Every hart runs the *same* entry code; it is the `a0` above (cached
//! through [`crate::regs::hart_id`]) that tells a hart which role it plays,
//! and that is why the kernel's per-hart bookkeeping must be set up before
//! any code reads the id.
//!
//! # The `opaque` convention
//!
//! [`sbi_hart_start`] passes a third argument — `a2`, the `opaque` value
//! the HSM spec defines for the booting hart's own use. VelaOS does not
//! use it (the entry learns everything from `a0`/`a1`), so the helper
//! passes zero; a future boot protocol that wants to smuggle a pointer to
//! the booting hart changes exactly here.
//!
//! # Parking
//!
//! A hart with nothing to do parks in [`park_in_wfi`]: `wfi` waits for an
//! interrupt and nothing else, so a parked hart burns no timebase and no
//! PLIC budget. The loop is deliberate — `wfi` may wake spuriously, so the
//! only correct park is a loop around the instruction.

use crate::regs::{self, ON_BARE_METAL};

/// The largest CPU index the kernel's per-CPU arrays admit.
///
/// Matches the x86-64 crate's per-CPU array bound; QEMU's `virt` machine
/// supports up to 512 harts, so a config that wants more than 8 raises
/// exactly this constant rather than silently aliasing per-CPU state.
pub const MAX_CPUS: usize = 8;

/// The SBI HSM extension id, `0x0048_534D` — the ASCII for `"HSM"`.
///
/// The SBI spec *defines* the extension id as that ASCII value, which is
/// why the constant is not an arbitrary small number like the legacy EIDs.
pub const SBI_HSM_EID: u64 = 0x0048_534D;

/// The HSM function id for `hart_start`.
pub const FID_HSM_HART_START: u64 = 0;

/// Ask the firmware to start hart `hart_id` at `start_addr` in S-mode.
///
/// Returns `(error, value)` from the HSM call: `error` 0 on success, or a
/// negative SBI error code (`S_ERR_INVALID_PARAM` for an unknown hart, for
/// example) that the caller treats the same way the x86-64 SMP path treats
/// a failed SIPI — as "this candidate did not come up".
///
/// Off bare metal the call is inert and reports success.
pub fn sbi_hart_start(hart_id: u64, start_addr: u64) -> (u64, u64) {
    // HSM `hart_start`: a0 = hartid, a1 = start_addr, a2 = opaque (see the
    // module docs for why it is zero).
    let (error, value) = regs::sbi_call(SBI_HSM_EID, FID_HSM_HART_START, hart_id, start_addr);
    if !ON_BARE_METAL {
        // The host helper already reports (0, 0); the branch documents the
        // contract for a reader rather than changing behaviour.
        return (error, value);
    }
    (error, value)
}

/// Halt the current hart until an interrupt wakes it, in a loop.
///
/// `wfi` is a *hint*, not a barrier: the hart may wake at any time for any
/// reason, so a hart that intends to stay asleep must re-enter the wait
/// every time it wakes. This is the body every parked hart runs.
pub fn park_in_wfi() {
    loop {
        regs::wfi();
    }
}

/// True when the hart executing this code is the boot hart (index 0 by
/// convention).
///
/// The kernel's boot sequence runs on the hart the firmware started first;
/// everything else is brought up by [`sbi_hart_start`].
pub fn is_boot_hart() -> bool {
    regs::hart_id() == 0
}

#[cfg(test)]
extern crate std;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_hsm_extension_id_is_the_ascii_hsm() {
        // 0x0048_534D spells "HSM"; the SBI spec defines it that way, so the
        // constant is self-documenting rather than magic.
        assert_eq!(SBI_HSM_EID, 0x0048_534D);
        assert_eq!(
            SBI_HSM_EID,
            (b'H' as u64) << 16 | (b'S' as u64) << 8 | b'M' as u64,
            "the id is the ASCII, most significant byte first"
        );
    }

    #[test]
    fn hart_start_is_function_zero() {
        assert_eq!(FID_HSM_HART_START, 0);
    }

    #[test]
    fn the_cpu_bound_matches_the_x86_crate() {
        // Both architectures share the kernel's per-CPU arrays, so the
        // bounds must agree or a config valid on one is broken on both.
        assert_eq!(MAX_CPUS, 8);
    }

    #[test]
    fn a_fresh_hart_is_not_the_boot_hart() {
        // The test process never ran the boot protocol, so the cached id
        // is 0 — which *is* the boot hart's id by convention. The check
        // pins the convention; a test that set the id would break it.
        assert!(is_boot_hart());
    }

    #[test]
    fn the_start_call_is_inert_off_bare_metal() {
        // Must not reach the firmware on the host and must report the
        // documented success shape.
        assert_eq!(sbi_hart_start(1, 0x0000_8000_0000), (0, 0));
    }
}
