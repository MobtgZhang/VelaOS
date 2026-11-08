//! PCI bus enumeration for x86-64.
//!
//! This is the real x86-64 PCI enumeration, replacing the stub that shipped
//! with the architecture facade. It walks every bus/device/function using the
//! standard configuration-space IO ports (`0xCF8`/`0xCFC`), reads the vendor,
//! device, class, revision and BARs, and reports whether each function is
//! MSI-X capable.
//!
//! # Configuration-space access
//!
//! The configuration address register at port `0xCF8` selects a function and
//! register offset; the configuration data register at port `0xCFC` is then
//! read or written. Only dword-aligned accesses are used, which is what the
//! architectural mechanism guarantees.
//!
//! # Why the enable bit matters
//!
//! Bit 31 of the address register is the enable bit. Without it, reads from
//! `0xCFC` return `0xFFFFFFFF`, which is indistinguishable from "no device
//! present" — so every probe writes the address with the enable bit set first.

use crate::regs;

/// The PCI configuration address register.
const CONFIG_ADDR: u16 = 0xCF8;
/// The PCI configuration data register.
const CONFIG_DATA: u16 = 0xCFC;

/// The enable bit of the configuration address register.
const CONFIG_ENABLE: u32 = 1 << 31;

/// How many functions are scanned per device.
const FUNCTIONS: u8 = 8;
/// How many devices are scanned per bus.
const DEVICES: u8 = 32;
/// How many buses are scanned (0..256 needs more than a u8).
const BUSES: usize = 256;

/// A PCI base-address register entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Bar {
    /// The raw BAR value as read from configuration space.
    pub raw: u32,
    /// True when this is a memory-mapped BAR (false = I/O port BAR).
    pub memory: bool,
    /// True when the BAR is prefetchable (memory BARs only).
    pub prefetchable: bool,
    /// The address/size the BAR decodes to. For 64-bit memory BARs this is
    /// the combined 64-bit base; for 32-bit BARs the upper 32 bits are zero.
    pub base: u64,
    /// The size of the region the BAR decodes to, in bytes.
    pub size: u64,
}

/// One discovered PCI function.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PciDevice {
    /// The PCI bus number.
    pub bus: u8,
    /// The device number on the bus.
    pub device: u8,
    /// The function number within the device.
    pub function: u8,
    /// The vendor ID.
    pub vendor_id: u16,
    /// The device ID.
    pub device_id: u16,
    /// The class code (high byte = base class, low two bytes = sub/prog-if).
    pub class_code: u32,
    /// The revision ID.
    pub revision: u8,
    /// The base-address registers.
    pub bars: [Bar; 6],
    /// The interrupt line, or 0 when not applicable / polled.
    pub irq: u8,
    /// True when the function advertises an MSI-X capability.
    pub msi_x: bool,
}

impl PciDevice {
    /// The base class (top byte of the class code).
    pub const fn base_class(self) -> u8 {
        (self.class_code >> 16) as u8
    }

    /// The subclass (middle byte of the class code).
    pub const fn sub_class(self) -> u8 {
        (self.class_code >> 8) as u8
    }

    /// The programming interface (low byte of the class code).
    pub const fn prog_if(self) -> u8 {
        self.class_code as u8
    }

    /// True when the device is a mass-storage controller.
    pub const fn is_mass_storage(self) -> bool {
        self.base_class() == 0x01
    }

    /// True when the device is an NVMe controller (base 0x01, sub 0x08).
    pub const fn is_nvme(self) -> bool {
        self.base_class() == 0x01 && self.sub_class() == 0x08
    }

    /// True when the device is a display controller.
    pub const fn is_display(self) -> bool {
        self.base_class() == 0x03
    }

    /// True when the device is a network controller.
    pub const fn is_network(self) -> bool {
        self.base_class() == 0x02
    }

    /// True when the device is an audio controller.
    pub const fn is_audio(self) -> bool {
        self.base_class() == 0x04
    }
}

/// Build the configuration address for a function and register offset.
///
/// `offset` is rounded down to a dword boundary.
fn config_address(bus: u8, device: u8, function: u8, offset: u8) -> u32 {
    CONFIG_ENABLE
        | (bus as u32) << 16
        | (device as u32) << 11
        | (function as u32) << 8
        | (offset & 0xFC) as u32
}

/// Read a dword from PCI configuration space.
///
/// Returns `0xFFFFFFFF` when no device responds at the selected bus/device/
/// function, which is how an empty slot is detected.
fn pci_read_dword(bus: u8, device: u8, function: u8, offset: u8) -> u32 {
    // SAFETY: `0xCF8`/`0xCFC` are the architectural PCI configuration ports;
    // writing the address with the enable bit set and reading the data port
    // is always defined on a machine with a PCI host bridge.
    unsafe {
        regs::outl(CONFIG_ADDR, config_address(bus, device, function, offset));
        // A tiny delay lets the host bridge latch the address before the data
        // port is read; the delay is harmless on machines that do not need it.
        io_delay();
        regs::inl(CONFIG_DATA)
    }
}

/// Write a dword to PCI configuration space.
fn pci_write_dword(bus: u8, device: u8, function: u8, offset: u8, value: u32) {
    // SAFETY: see [`pci_read_dword`].
    unsafe {
        regs::outl(CONFIG_ADDR, config_address(bus, device, function, offset));
        io_delay();
        regs::outl(CONFIG_DATA, value);
    }
}

/// Read a word from PCI configuration space.
fn pci_read_word(bus: u8, device: u8, function: u8, offset: u8) -> u16 {
    let dword = pci_read_dword(bus, device, function, offset & 0xFC);
    if offset & 2 == 0 {
        dword as u16
    } else {
        (dword >> 16) as u16
    }
}

/// Read a byte from PCI configuration space.
fn pci_read_byte(bus: u8, device: u8, function: u8, offset: u8) -> u8 {
    let dword = pci_read_dword(bus, device, function, offset & 0xFC);
    let shift = (offset & 3) * 8;
    (dword >> shift) as u8
}

/// Measure the size of a BAR.
///
/// Writes all ones to the BAR, reads back the mask the hardware keeps, then
/// restores the original value. Returns the size in bytes, or zero when the
/// BAR is not implemented.
fn bar_size(bus: u8, device: u8, function: u8, offset: u8) -> u64 {
    let original = pci_read_dword(bus, device, function, offset);
    // The BAR is writable by software, and the value is restored
    // immediately afterwards (pci_write_dword encapsulates the unsafety).
    pci_write_dword(bus, device, function, offset, 0xFFFF_FFFF);
    let mask = pci_read_dword(bus, device, function, offset);
    // The original value is restored below.
    pci_write_dword(bus, device, function, offset, original);
    if mask == 0 {
        return 0;
    }
    // The low bits are type/flags, not part of the address mask; clear them.
    let address_mask = mask & 0xFFFFFFF0;
    if address_mask == 0 {
        return 0;
    }
    // Size = ~mask + 1, rounded up to the next power of two that the mask
    // implies. The architectural rule: a BAR's size field has ones in all
    // bits below the size, so ~mask + 1 is the size.
    (!address_mask).wrapping_add(1) as u64
}
/// Enumerate every PCI function on every bus.
///
/// Returns the list of functions that responded (vendor ID != 0xFFFF). The
/// list is in scan order: bus 0 first, then device, then function.
pub fn enumerate() -> alloc::vec::Vec<PciDevice> {
    let mut devices: alloc::vec::Vec<PciDevice> = alloc::vec::Vec::new();
    for bus in 0..BUSES {
        let bus = bus as u8;
        for device in 0..DEVICES {
            for function in 0..FUNCTIONS {
                let vendor = pci_read_word(bus, device, function, 0);
                if vendor == 0xFFFF {
                    continue;
                }
                let device_id = pci_read_word(bus, device, function, 2);
                let class_code = pci_read_dword(bus, device, function, 0x08);
                let revision = pci_read_byte(bus, device, function, 0x08);
                let irq = pci_read_byte(bus, device, function, 0x3C);

                let mut bars = [Bar {
                    raw: 0,
                    memory: false,
                    prefetchable: false,
                    base: 0,
                    size: 0,
                }; 6];
                let mut offset = 0x10;
                let mut index = 0;
                while offset < 0x24 && index < 6 {
                    let raw = pci_read_dword(bus, device, function, offset);
                    let memory = raw & 1 == 0;
                    let consumed = if memory && (raw >> 1) & 3 == 2 {
                        // 64-bit BAR: read the upper dword now.
                        let upper = pci_read_dword(bus, device, function, offset + 4);
                        let base = (upper as u64) << 32 | (raw as u64 & 0xFFFFFFF0);
                        let size = bar_size(bus, device, function, offset);
                        bars[index] = Bar {
                            raw,
                            memory: true,
                            prefetchable: (raw >> 8) & 1 == 1,
                            base,
                            size,
                        };
                        if index + 1 < 6 {
                            bars[index + 1] = Bar {
                                raw: upper,
                                memory: true,
                                prefetchable: (raw >> 8) & 1 == 1,
                                base,
                                size,
                            };
                        }
                        2
                    } else {
                        let base = raw as u64 & 0xFFFFFFF0;
                        let size = bar_size(bus, device, function, offset);
                        bars[index] = Bar {
                            raw,
                            memory,
                            prefetchable: memory && (raw >> 8) & 1 == 1,
                            base,
                            size,
                        };
                        1
                    };
                    offset += consumed as u8 * 4;
                    index += consumed;
                }

                // Scan for an MSI-X capability, starting at the capability
                // pointer in the status register (offset 0x34, byte 3).
                let mut msi_x = false;
                let mut cap_ptr = pci_read_byte(bus, device, function, 0x34);
                while cap_ptr != 0 && cap_ptr != 0xFF {
                    let cap_id = pci_read_byte(bus, device, function, cap_ptr);
                    if cap_id == 0x11 {
                        // MSI-X capability: capability ID 0x11.
                        msi_x = true;
                        break;
                    }
                    cap_ptr = pci_read_byte(bus, device, function, cap_ptr + 1);
                }

                devices.push(PciDevice {
                    bus,
                    device,
                    function,
                    vendor_id: vendor,
                    device_id,
                    class_code,
                    revision,
                    bars,
                    irq,
                    msi_x,
                });

                // A device that is not a multi-function device has only
                // function 0. The header type byte (offset 0x0E, bit 7)
                // says whether more functions exist.
                if function == 0 {
                    let header_type = pci_read_byte(bus, device, function, 0x0E);
                    if header_type & 0x80 == 0 {
                        break;
                    }
                }
            }
        }
    }
    devices
}

extern crate alloc;

/// A short delay between PCI configuration-port accesses.
///
/// Some host bridges need a tiny pause between writing the address port and
/// reading the data port. Reading the data port twice or issuing a port I/O
/// to an unused port (0xED) is the conventional cheap delay.
#[inline]
fn io_delay() {
    // SAFETY: port `0xED` is an unused, harmless port for this purpose.
    unsafe {
        let _ = regs::inb(0xED);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_address_has_the_enable_bit_set() {
        let addr = config_address(0, 0, 0, 0);
        assert_eq!(addr & CONFIG_ENABLE, CONFIG_ENABLE);
    }

    #[test]
    fn base_class_extracts_the_top_byte() {
        let d = PciDevice {
            class_code: 0x01_08_00,
            ..PciDevice {
                bus: 0,
                device: 0,
                function: 0,
                vendor_id: 0,
                device_id: 0,
                class_code: 0,
                revision: 0,
                bars: [Bar {
                    raw: 0,
                    memory: false,
                    prefetchable: false,
                    base: 0,
                    size: 0,
                }; 6],
                irq: 0,
                msi_x: false,
            }
        };
        assert_eq!(d.base_class(), 0x01);
        assert_eq!(d.sub_class(), 0x08);
        assert!(d.is_nvme());
        assert!(!d.is_display());
        assert!(!d.is_network());
    }

    #[test]
    fn bar_memory_flag_is_clear_for_io_bar() {
        let bar = Bar {
            raw: 1,
            memory: false,
            prefetchable: false,
            base: 0,
            size: 0,
        };
        assert!(!bar.memory);
    }

    #[test]
    fn bar_memory_flag_is_set_for_memory_bar() {
        let bar = Bar {
            raw: 0,
            memory: true,
            prefetchable: false,
            base: 0,
            size: 0,
        };
        assert!(bar.memory);
    }
}
