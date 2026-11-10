//! Why LoongArch has no global descriptor table.
//!
//! The x86-64 architecture has a *global descriptor table*: a memory-resident
//! table of segment descriptors that the processor consults on every
//! privilege change and every far call. It is part of the x86-64 programming
//! model, and the x86-64 crate in this project installs one.
//!
//! LoongArch has no such table. The architecture's privilege levels are
//! described by two bits in `CSR.CRMD` and two bits in the `PRMD` saved
//! state; there is no descriptor table, no segment selector and no far
//! call. The closest thing to a "descriptor" is the direct mapping window
//! (`CSR.DMW0`–`DMW3`), and those are windows, not descriptors: they map a
//! physical range into a virtual segment without any per-task state.
//!
//! # Why this module exists at all
//!
//! The architecture facade re-exports the same module names for every
//! architecture, so that the code above it never has to know which one it
//! is running on. A module that explains the absence is more useful than a
//! module that is missing: a reader who wonders "where is the GDT?" gets
//! the answer here rather than a compile error.

/// The number of privilege levels the architecture defines.
///
/// LoongArch defines four: PLV 0 (kernel), 1, 2 and 3 (user). VelaOS uses
/// the kernel level and the user level.
pub const PRIVILEGE_LEVELS: u32 = 4;

/// The kernel's privilege level.
pub const PLV_KERNEL: u64 = 0;

/// The user privilege level.
pub const PLV_USER: u64 = 3;

/// True when this architecture has a global descriptor table.
///
/// It does not, and the kernel's architecture-neutral code must not assume
/// that it does: the descriptor table is an x86 concept, and the equivalent
/// state on LoongArch is the privilege level in `CSR.CRMD` and the direct
/// mapping windows.
pub const HAS_GDT: bool = false;

#[cfg(test)]
extern crate std;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_privilege_levels_are_the_architectures() {
        assert_eq!(PRIVILEGE_LEVELS, 4);
        assert_eq!(PLV_KERNEL, 0);
        assert_eq!(PLV_USER, 3);
    }
}
