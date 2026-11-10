//! The identifier of the core that is running.
//!
//! The core number does not live in a register a hart can read: it comes
//! from the boot tables the firmware hands to the first hart. The kernel
//! therefore publishes it once, at boot, and every hart reads the copy
//! that belongs to it.
//!
//! The x86-64 crate reads the number from a per-CPU APIC; the LoongArch
//! crate reads it from the value the boot code stored, which is the same
//! idea reached by a different road.

use crate::smp;

/// The core number of the hart that is running.
pub fn current_cpu_id() -> u64 {
    smp::current_core_id() as u64
}

/// Publish the core number of this hart.
///
/// The boot code calls this once per hart: the number comes from the
/// firmware's tables, and every hart has to be told which one it is.
pub fn set(index: usize) {
    smp::set_current_core_id(index as u32);
}

#[cfg(test)]
extern crate std;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_identifier_is_the_boot_published_one() {
        smp::set_current_core_id(2);
        assert_eq!(current_cpu_id(), 2);
        smp::set_current_core_id(0);
        assert_eq!(current_cpu_id(), 0);
    }
}
