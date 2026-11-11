//! Bounds-checked USB device and configuration descriptor parsing.

use alloc::vec::Vec;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BootProtocol {
    Keyboard,
    Mouse,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct BootInterface {
    pub(crate) interface: u8,
    pub(crate) protocol: BootProtocol,
    pub(crate) endpoint: u8,
    pub(crate) max_packet: u16,
    pub(crate) interval: u8,
    pub(crate) configuration: u8,
}

/// Extract boot keyboard/mouse interrupt-IN endpoints from one configuration.
///
/// Descriptors are walked by their declared lengths. Truncated, zero-length,
/// and overrun descriptors terminate the scan rather than causing an out-of-
/// bounds read. Only alternate setting zero is accepted, as required by the
/// simple boot protocol path used here.
pub(crate) fn boot_interfaces(bytes: &[u8]) -> Vec<BootInterface> {
    let mut interfaces = Vec::new();
    if bytes.len() < 9 || bytes[0] < 9 || bytes[1] != 2 {
        return interfaces;
    }
    let configuration = bytes[5];
    let total = u16::from_le_bytes([bytes[2], bytes[3]]) as usize;
    let limit = total.min(bytes.len());
    let mut offset = 0usize;
    let mut current: Option<(u8, BootProtocol)> = None;

    while offset + 2 <= limit {
        let length = bytes[offset] as usize;
        if length < 2 || offset + length > limit {
            break;
        }
        let kind = bytes[offset + 1];
        match kind {
            4 if length >= 9 => {
                let class = bytes[offset + 5];
                let subclass = bytes[offset + 6];
                let protocol = bytes[offset + 7];
                let alternate = bytes[offset + 2];
                current = if class == 3 && subclass == 1 && alternate == 0 {
                    match protocol {
                        1 => Some((bytes[offset + 1], BootProtocol::Keyboard)),
                        2 => Some((bytes[offset + 1], BootProtocol::Mouse)),
                        _ => None,
                    }
                } else {
                    None
                };
            }
            5 if length >= 7 => {
                if let Some((interface, protocol)) = current {
                    let address = bytes[offset + 2];
                    let attributes = bytes[offset + 3];
                    let max_packet = u16::from_le_bytes([bytes[offset + 4], bytes[offset + 5]])
                        & 0x07ff;
                    if address & 0x80 != 0 && attributes & 3 == 3 && max_packet != 0 {
                        let duplicate = interfaces.iter().any(|item: &BootInterface| {
                            item.interface == interface && item.endpoint == address
                        });
                        if !duplicate {
                            interfaces.push(BootInterface {
                                interface,
                                protocol,
                                endpoint: address,
                                max_packet,
                                interval: bytes[offset + 6].max(1),
                                configuration,
                            });
                        }
                    }
                }
            }
            _ => {}
        }
        offset += length;
    }
    interfaces
}

pub(crate) fn device_max_packet(device: &[u8], super_speed: bool) -> Option<u16> {
    if device.len() < 8 || device[0] < 8 || device[1] != 1 {
        return None;
    }
    let value = if super_speed && device[7] == 9 {
        512
    } else {
        device[7] as u16
    };
    match value {
        8 | 16 | 32 | 64 | 512 => Some(value),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn configuration(protocol: u8) -> Vec<u8> {
        vec![
            9, 2, 32, 0, 1, 1, 0, 0x80, 50, // configuration
            9, 4, 0, 0, 1, 3, 1, protocol, 0, // boot interface
            9, 0x21, 0x11, 0x01, 0, 1, 0x22, 52, 0, // HID
            7, 5, 0x81, 3, 8, 0, 10, // interrupt IN
        ]
    }

    #[test]
    fn finds_a_boot_keyboard_endpoint() {
        let parsed = boot_interfaces(&configuration(1));
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].protocol, BootProtocol::Keyboard);
        assert_eq!(parsed[0].endpoint, 0x81);
        assert_eq!(parsed[0].max_packet, 8);
        assert_eq!(parsed[0].interval, 10);
        assert_eq!(parsed[0].configuration, 1);
    }

    #[test]
    fn finds_a_mouse_and_rejects_an_output_endpoint() {
        let mut bytes = configuration(2);
        let total = bytes.len();
        bytes[2] = total as u8;
        bytes[3] = (total >> 8) as u8;
        let parsed = boot_interfaces(&bytes);
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].protocol, BootProtocol::Mouse);
        bytes[25] = 0x01;
        assert!(boot_interfaces(&bytes).is_empty());
    }

    #[test]
    fn malformed_lengths_do_not_escape_the_descriptor_buffer() {
        assert!(boot_interfaces(&[9, 2, 40, 0]).is_empty());
        assert!(boot_interfaces(&[9, 2, 9, 0, 0, 1, 0, 0, 0, 0, 4]).is_empty());
        assert!(boot_interfaces(&[0, 2]).is_empty());
    }

    #[test]
    fn non_boot_hid_interfaces_are_ignored() {
        let mut bytes = configuration(1);
        bytes[15] = 0;
        assert!(boot_interfaces(&bytes).is_empty());
    }

    #[test]
    fn reads_the_device_ep0_packet_size() {
        let mut descriptor = [0u8; 18];
        descriptor[0] = 18;
        descriptor[1] = 1;
        descriptor[7] = 64;
        assert_eq!(device_max_packet(&descriptor, false), Some(64));
        descriptor[7] = 7;
        assert_eq!(device_max_packet(&descriptor, false), None);
    }
}
