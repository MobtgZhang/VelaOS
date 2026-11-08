//! PCI bus enumeration placeholder.
//!
//! VelaOS does not yet enumerate the PCI bus or expose a DMA API; both are
//! tracked as genuine gaps in `reports/issues-001.md`. This module exists so
//! the architecture facade can export a stable `pci` surface and so
//! architecture-neutral code can refer to `vela_arch::pci` without failing to
//! compile. The real x86-64 PCI enumeration (MCFG, config-space reads, BAR
//! parsing, MSI-X) is a future milestone.

/// A PCI device identifier, once enumeration exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeviceId(pub u16, pub u16); // (vendor, device)

impl DeviceId {
    /// No device: the sentinel used before enumeration runs.
    pub const fn none() -> Self {
        Self(0xFFFF, 0xFFFF)
    }
}

/// Enumerate the PCI bus and return the devices found.
///
/// Not yet implemented: returns an empty list. Callers must handle the empty
/// result gracefully, because a machine may genuinely have no enumerable PCI
/// devices, and enumeration itself is not yet implemented.
pub fn enumerate() -> alloc::vec::Vec<DeviceId> {
    alloc::vec::Vec::new()
}

extern crate alloc;
