//! The identifier of the CPU that is running.
//!
//! The hardware identifier is `MPIDR_EL1.Aff0`, and the kernel's scheduler
//! works with dense *indices* into its per-CPU arrays. The boot code
//! installs the mapping once, at boot, and every hart reads the copy that
//! belongs to it.

use crate::smp;

/// The index of this CPU into the per-CPU arrays.
pub fn current_cpu_id() -> usize {
    smp::current_cpu_index()
}

/// Publish the index of this CPU.
///
/// The boot code calls this once per hart, before the scheduler considers
/// the hart runnable.
pub fn set(index: usize) {
    smp::set_current_cpu_index(index);
}

#[cfg(test)]
extern crate std;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_index_round_trips() {
        smp::set_current_cpu_index(2);
        assert_eq!(current_cpu_id(), 2);
        smp::set_current_cpu_index(0);
        assert_eq!(current_cpu_id(), 0);
    }
}
