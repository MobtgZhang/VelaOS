//! Why AArch64 has no global descriptor table.
//!
//! The x86-64 architecture has a *global descriptor table*: a
//! memory-resident table of segment descriptors that the processor consults
//! on every privilege change and every far call. It is part of the x86-64
//! programming model, and the x86-64 crate in this project installs one.
//!
//! AArch64 has no such table. Privilege is described by the current
//! exception level, which lives in `CurrentEL`, and the saved state in
//! `SPSR_EL1`. There are no segment selectors and no far calls: the
//! instruction set has no need of them.
//!
//! # Why this module exists at all
//!
//! The architecture facade re-exports the same module names for every
//! architecture, so that the code above it never has to know which one it
//! is running on. A module that explains the absence is more useful than a
//! module that is missing: a reader who wonders "where is the GDT?" gets
//! the answer here rather than a compile error.

/// The number of exception levels the architecture defines.
///
/// AArch64 defines four: EL0 (user), EL1 (kernel), EL2 (hypervisor) and
/// EL3 (secure monitor). VelaOS uses EL0 and EL1.
pub const EXCEPTION_LEVELS: u32 = 4;

/// The user exception level.
pub const EL_USER: u64 = 0;

/// The kernel exception level.
pub const EL_KERNEL: u64 = 1;

/// True when this architecture has a global descriptor table.
///
/// It does not, and the kernel's architecture-neutral code must not assume
/// that it does: the descriptor table is an x86 concept, and the equivalent
/// state on AArch64 is the exception level.
pub const HAS_GDT: bool = false;

#[cfg(test)]
extern crate std;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_exception_levels_are_the_architectures() {
        assert_eq!(EXCEPTION_LEVELS, 4);
        assert_eq!(EL_USER, 0);
        assert_eq!(EL_KERNEL, 1);
    }
}
