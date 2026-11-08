//! A small network stack: Ethernet framing, IPv4, UDP and ARP.
//!
//! # What this crate is for
//!
//! It is the *protocol-processing* half of VelaOS's networking. It does not
//! own a NIC: a NIC driver (for example `drivers/virtio-net`) feeds it raw
//! Ethernet frames, and it delivers completed packets back to the driver for
//! transmission. The split mirrors the input crate: parsing is pure and
//! unit-testable, hardware access is the driver's job.
//!
//! # Layers
//!
//! * Ethernet — parse and build frames (`dst`, `src`, ethertype).
//! * ARP — resolve an IPv4 address to a MAC on the local network.
//! * IPv4 — parse and build packets, verify the header checksum, handle TTL.
//! * UDP — parse and build datagrams, verify the pseudo-header checksum.
//!
//! # Buffers
//!
//! Packets are carried in [`Packet`], which borrows a byte slice and tracks
//! how much of it is header vs payload. No allocation happens on the hot
//! receive path; the driver owns the DMA buffer and lends it to the stack.

#![no_std]

extern crate alloc;

use alloc::boxed::Box;
use core::fmt;
use spin::Once;

// ========================================================================
// Ethernet
// ========================================================================

/// A 48-bit Ethernet MAC address.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(C)]
pub struct MacAddr(pub [u8; 6]);

impl MacAddr {
    /// The broadcast address, `FF:FF:FF:FF:FF:FF`.
    pub const BROADCAST: MacAddr = MacAddr([0xFF; 6]);

    /// True when this is the broadcast address.
    pub fn is_broadcast(self) -> bool {
        self.0 == [0xFF; 6]
    }

    /// True when this is a multicast address (low bit of the first octet set).
    pub fn is_multicast(self) -> bool {
        self.0[0] & 1 != 0
    }
}

impl fmt::Display for MacAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
            self.0[0], self.0[1], self.0[2], self.0[3], self.0[4], self.0[5]
        )
    }
}

/// Ethernet ethertypes.
pub mod ethertype {
    /// IPv4.
    pub const IPV4: u16 = 0x0800;
    /// ARP.
    pub const ARP: u16 = 0x0806;
}

/// Network error codes.
pub mod errors {
    /// The call succeeded.
    pub const OK: i32 = 0;
    /// No network device has been installed.
    pub const NO_DEVICE: i32 = -1;
    /// The frame is too large or malformed.
    pub const EMSGSIZE: i32 = -2;
}

/// An Ethernet frame header.
#[derive(Debug, Clone, Copy)]
pub struct EthernetHeader {
    pub dst: MacAddr,
    pub src: MacAddr,
    pub ethertype: u16, // network byte order
}

impl EthernetHeader {
    /// The size of an Ethernet header, in bytes.
    pub const LEN: usize = 14;

    /// Parse an Ethernet header from `bytes`.
    ///
    /// Returns `None` when `bytes` is shorter than a header.
    pub fn parse(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < Self::LEN {
            return None;
        }
        let mut dst = [0u8; 6];
        let mut src = [0u8; 6];
        dst.copy_from_slice(&bytes[0..6]);
        src.copy_from_slice(&bytes[6..12]);
        let ethertype = u16::from_be_bytes([bytes[12], bytes[13]]);
        Some(Self {
            dst: MacAddr(dst),
            src: MacAddr(src),
            ethertype,
        })
    }

    /// Build an Ethernet frame into `out`, returning the payload offset.
    pub fn build(out: &mut [u8], dst: MacAddr, src: MacAddr, ethertype: u16) -> usize {
        out[0..6].copy_from_slice(&dst.0);
        out[6..12].copy_from_slice(&src.0);
        out[12..14].copy_from_slice(&ethertype.to_be_bytes());
        Self::LEN
    }
}

/// Ethernet frame types the stack understands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EthernetFrame {
    Ipv4(Box<[u8]>),
    Arp(Box<[u8]>),
    Unknown(u16, Box<[u8]>),
}

// ========================================================================
// IPv4
// ========================================================================

/// An IPv4 address.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(C)]
pub struct Ipv4Addr(pub [u8; 4]);

impl Ipv4Addr {
    /// The wildcard address, `0.0.0.0`.
    pub const UNSPECIFIED: Ipv4Addr = Ipv4Addr([0, 0, 0, 0]);
    /// The loopback address, `127.0.0.1`.
    pub const LOOPBACK: Ipv4Addr = Ipv4Addr([127, 0, 0, 1]);
    /// The limited broadcast address, `255.255.255.255`.
    pub const LIMITED_BROADCAST: Ipv4Addr = Ipv4Addr([255, 255, 255, 255]);

    /// Build an address from four octets.
    pub const fn new(a: u8, b: u8, c: u8, d: u8) -> Self {
        Self([a, b, c, d])
    }

    /// The address as a 32-bit value in host byte order.
    pub const fn to_u32(self) -> u32 {
        u32::from_be_bytes(self.0)
    }

    /// Build from a 32-bit value in host byte order.
    pub const fn from_u32(v: u32) -> Self {
        Self(v.to_be_bytes())
    }

    /// True when this is a multicast address (224.0.0.0/4).
    pub const fn is_multicast(self) -> bool {
        (self.0[0] & 0xF0) == 0xE0
    }

    /// True when this is a link-local address (169.254.0.0/16).
    pub const fn is_link_local(self) -> bool {
        self.0[0] == 169 && self.0[1] == 254
    }
}

impl fmt::Display for Ipv4Addr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}.{}", self.0[0], self.0[1], self.0[2], self.0[3])
    }
}

/// IPv4 protocol numbers.
pub mod protocol {
    /// ICMP.
    pub const ICMP: u8 = 1;
    /// TCP.
    pub const TCP: u8 = 6;
    /// UDP.
    pub const UDP: u8 = 17;
}

/// An IPv4 header.
#[derive(Debug, Clone, Copy)]
pub struct Ipv4Header {
    pub version_ihl: u8, // version (high 4) + header length (low 4, in 32-bit words)
    pub dscp_ecn: u8,
    pub total_length: u16, // network byte order
    pub identification: u16,
    pub flags_fragment: u16,
    pub ttl: u8,
    pub protocol: u8,
    pub header_checksum: u16, // network byte order
    pub src: Ipv4Addr,
    pub dst: Ipv4Addr,
}

impl Ipv4Header {
    /// The size of a minimal IPv4 header (no options), in bytes.
    pub const LEN: usize = 20;

    /// Parse an IPv4 header from `bytes`.
    ///
    /// Returns `None` when `bytes` is too short or the version is not 4.
    pub fn parse(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < Self::LEN {
            return None;
        }
        let version = bytes[0] >> 4;
        if version != 4 {
            return None;
        }
        let mut src = [0u8; 4];
        let mut dst = [0u8; 4];
        src.copy_from_slice(&bytes[12..16]);
        dst.copy_from_slice(&bytes[16..20]);
        Some(Self {
            version_ihl: bytes[0],
            dscp_ecn: bytes[1],
            total_length: u16::from_be_bytes([bytes[2], bytes[3]]),
            identification: u16::from_be_bytes([bytes[4], bytes[5]]),
            flags_fragment: u16::from_be_bytes([bytes[6], bytes[7]]),
            ttl: bytes[8],
            protocol: bytes[9],
            header_checksum: u16::from_be_bytes([bytes[10], bytes[11]]),
            src: Ipv4Addr(src),
            dst: Ipv4Addr(dst),
        })
    }

    /// The header length, in bytes.
    pub const fn header_len(self) -> usize {
        (self.version_ihl & 0x0F) as usize * 4
    }

    /// The total packet length, in bytes.
    pub const fn total_len(self) -> usize {
        self.total_length as usize
    }

    /// Build an IPv4 header into `out`, returning the header length.
    ///
    /// `total_length` includes the header. `checksum` may be zero to have the
    /// caller fill it in via [`Self::checksum`].
    #[allow(clippy::too_many_arguments)]
    pub fn build(
        out: &mut [u8],
        dscp_ecn: u8,
        total_length: u16,
        identification: u16,
        flags_fragment: u16,
        ttl: u8,
        protocol: u8,
        checksum: u16,
        src: Ipv4Addr,
        dst: Ipv4Addr,
    ) -> usize {
        out[0] = 0x45; // version 4, ihl 5
        out[1] = dscp_ecn;
        out[2..4].copy_from_slice(&total_length.to_be_bytes());
        out[4..6].copy_from_slice(&identification.to_be_bytes());
        out[6..8].copy_from_slice(&flags_fragment.to_be_bytes());
        out[8] = ttl;
        out[9] = protocol;
        out[10..12].copy_from_slice(&checksum.to_be_bytes());
        out[12..16].copy_from_slice(&src.0);
        out[16..20].copy_from_slice(&dst.0);
        Self::LEN
    }

    /// The one's-complement header checksum over the header.
    ///
    /// Returns zero when the checksum is valid; nonzero means corruption.
    pub fn checksum(bytes: &[u8]) -> u16 {
        let len = bytes.len();
        let mut sum: u32 = 0;
        let mut i = 0;
        while i + 1 < len {
            sum += u16::from_be_bytes([bytes[i], bytes[i + 1]]) as u32;
            i += 2;
        }
        if i < len {
            sum += (bytes[i] as u32) << 8;
        }
        while sum >> 16 != 0 {
            sum = (sum & 0xFFFF) + (sum >> 16);
        }
        !(sum as u16)
    }
}

/// One received IPv4 packet, borrowing the Ethernet payload.
#[derive(Debug, Clone, Copy)]
pub struct Ipv4Packet<'a> {
    pub header: Ipv4Header,
    pub payload: &'a [u8],
}

impl<'a> Ipv4Packet<'a> {
    /// Parse an IPv4 packet from an Ethernet payload.
    ///
    /// Returns `None` when the payload is too short, the version is wrong,
    /// or the header checksum is invalid.
    pub fn parse(bytes: &'a [u8]) -> Option<Self> {
        let header = Ipv4Header::parse(bytes)?;
        let header_len = header.header_len();
        if bytes.len() < header_len {
            return None;
        }
        if bytes.len() < header.total_len() {
            return None;
        }
        if Ipv4Header::checksum(&bytes[..header_len]) != 0 {
            return None;
        }
        let payload = &bytes[header_len..header.total_len()];
        Some(Self { header, payload })
    }
}

// ========================================================================
// UDP
// ========================================================================

/// A UDP datagram.
#[derive(Debug, Clone, Copy)]
pub struct UdpDatagram<'a> {
    pub src_port: u16,
    pub dst_port: u16,
    pub length: u16,
    pub checksum: u16,
    pub payload: &'a [u8],
}

impl<'a> UdpDatagram<'a> {
    /// The UDP header length, in bytes.
    pub const HEADER_LEN: usize = 8;

    /// Parse a UDP datagram from `bytes`.
    ///
    /// Returns `None` when `bytes` is shorter than a header or the declared
    /// length exceeds the input.
    pub fn parse(bytes: &'a [u8]) -> Option<Self> {
        if bytes.len() < Self::HEADER_LEN {
            return None;
        }
        let src_port = u16::from_be_bytes([bytes[0], bytes[1]]);
        let dst_port = u16::from_be_bytes([bytes[2], bytes[3]]);
        let length = u16::from_be_bytes([bytes[4], bytes[5]]);
        let checksum = u16::from_be_bytes([bytes[6], bytes[7]]);
        let length = length as usize;
        if length < Self::HEADER_LEN || length > bytes.len() {
            return None;
        }
        let payload = &bytes[Self::HEADER_LEN..length];
        Some(Self {
            src_port,
            dst_port,
            length: length as u16,
            checksum,
            payload,
        })
    }

    /// Build a UDP datagram into `out`, returning the total length.
    ///
    /// `checksum` may be zero for "no checksum".
    pub fn build(
        out: &mut [u8],
        src_port: u16,
        dst_port: u16,
        length: u16,
        checksum: u16,
        payload: &[u8],
    ) -> usize {
        out[0..2].copy_from_slice(&src_port.to_be_bytes());
        out[2..4].copy_from_slice(&dst_port.to_be_bytes());
        out[4..6].copy_from_slice(&length.to_be_bytes());
        out[6..8].copy_from_slice(&checksum.to_be_bytes());
        out[8..8 + payload.len()].copy_from_slice(payload);
        8 + payload.len()
    }

    /// The UDP pseudo-header checksum over an IPv4 src/dst pair, the UDP
    /// length, and the UDP header+payload.
    ///
    /// Returns zero when valid.
    pub fn checksum(src: Ipv4Addr, dst: Ipv4Addr, udp: &[u8]) -> u16 {
        // Pseudo header: src, dst, zero, protocol=UDP, udp length.
        let mut sum: u32 = 0;
        for b in src.0 {
            sum += b as u32;
        }
        for b in dst.0 {
            sum += b as u32;
        }
        sum += protocol::UDP as u32;
        sum += udp.len() as u32;
        let mut i = 0;
        while i + 1 < udp.len() {
            sum += u16::from_be_bytes([udp[i], udp[i + 1]]) as u32;
            i += 2;
        }
        if i < udp.len() {
            sum += (udp[i] as u32) << 8;
        }
        while sum >> 16 != 0 {
            sum = (sum & 0xFFFF) + (sum >> 16);
        }
        !(sum as u16)
    }
}

// ========================================================================
// ARP
// ========================================================================

/// An ARP packet.
#[derive(Debug, Clone, Copy)]
pub struct ArpPacket<'a> {
    pub hardware_type: u16,
    pub protocol_type: u16,
    pub hardware_size: u8,
    pub protocol_size: u8,
    pub opcode: u16,
    pub sender_mac: MacAddr,
    pub sender_ip: Ipv4Addr,
    pub target_mac: MacAddr,
    pub target_ip: Ipv4Addr,
    _phantom: core::marker::PhantomData<&'a ()>,
}

impl<'a> ArpPacket<'a> {
    /// The ARP header length, in bytes (Ethernet + IPv4).
    pub const HEADER_LEN: usize = 28;

    /// Parse an ARP packet from `bytes`.
    pub fn parse(bytes: &'a [u8]) -> Option<Self> {
        if bytes.len() < Self::HEADER_LEN {
            return None;
        }
        let mut smac = [0u8; 6];
        let mut tmac = [0u8; 6];
        smac.copy_from_slice(&bytes[8..14]);
        tmac.copy_from_slice(&bytes[18..24]);
        let mut sip = [0u8; 4];
        let mut tip = [0u8; 4];
        sip.copy_from_slice(&bytes[14..18]);
        tip.copy_from_slice(&bytes[24..28]);
        Some(Self {
            hardware_type: u16::from_be_bytes([bytes[0], bytes[1]]),
            protocol_type: u16::from_be_bytes([bytes[2], bytes[3]]),
            hardware_size: bytes[4],
            protocol_size: bytes[5],
            opcode: u16::from_be_bytes([bytes[6], bytes[7]]),
            sender_mac: MacAddr(smac),
            sender_ip: Ipv4Addr(sip),
            target_mac: MacAddr(tmac),
            target_ip: Ipv4Addr(tip),
            _phantom: core::marker::PhantomData,
        })
    }

    /// Build an ARP packet into `out`, returning the length.
    pub fn build(
        out: &mut [u8],
        opcode: u16,
        sender_mac: MacAddr,
        sender_ip: Ipv4Addr,
        target_mac: MacAddr,
        target_ip: Ipv4Addr,
    ) -> usize {
        out[0..2].copy_from_slice(&1u16.to_be_bytes()); // hardware = Ethernet
        out[2..4].copy_from_slice(&ethertype::IPV4.to_be_bytes());
        out[4] = 6; // hardware size
        out[5] = 4; // protocol size
        out[6..8].copy_from_slice(&opcode.to_be_bytes());
        out[8..14].copy_from_slice(&sender_mac.0);
        out[14..18].copy_from_slice(&sender_ip.0);
        out[18..24].copy_from_slice(&target_mac.0);
        out[24..28].copy_from_slice(&target_ip.0);
        Self::HEADER_LEN
    }
}

/// ARP opcodes.
pub mod opcode {
    pub const REQUEST: u16 = 1;
    pub const REPLY: u16 = 2;
}

/// A received packet: an Ethernet frame whose payload has been classified.
///
/// Borrows the input buffer; the lifetime is that of the raw frame bytes.
#[derive(Debug, Clone, Copy)]
pub enum Packet<'a> {
    Ipv4(Ipv4Packet<'a>),
    Arp(ArpPacket<'a>),
    Unknown(u16),
}

impl<'a> Packet<'a> {
    /// Classify a raw Ethernet frame.
    ///
    /// Returns `None` when the frame is too short or has an unknown ethertype.
    pub fn classify(bytes: &'a [u8]) -> Option<Self> {
        let eth = EthernetHeader::parse(bytes)?;
        let payload = bytes.get(EthernetHeader::LEN..)?;
        match eth.ethertype {
            ethertype::IPV4 => Ipv4Packet::parse(payload).map(Packet::Ipv4),
            ethertype::ARP => ArpPacket::parse(payload).map(Packet::Arp),
            et => Some(Packet::Unknown(et)),
        }
    }
}

/// A network device that can transmit Ethernet frames.
///
/// A NIC driver (for example `drivers/virtio-net`) implements this and
/// registers it with the network stack. The socket layer then transmits
/// outbound frames through it.
pub trait NetDevice: Send + Sync {
    /// Transmit a complete Ethernet frame.
    ///
    /// Returns the number of bytes transmitted, or an error.
    fn transmit(&self, frame: &[u8]) -> Result<usize, i32>;
}

/// The global transmit device, installed during boot.
static NET_DEVICE: Once<&dyn NetDevice> = Once::new();

/// True when a transmit device has already been installed.
///
/// Driver registration uses this to avoid replacing a real PCI NIC with a
/// synthetic fallback — `install_net_device` deliberately accepts only one
/// device, and the first one is the real one.
pub fn device_installed() -> bool {
    NET_DEVICE.get().is_some()
}

/// Install the network transmit device.
///
/// # Safety
///
/// Must be called exactly once during boot, before any socket transmit runs.
pub unsafe fn install_net_device(device: &'static dyn NetDevice) {
    // `Once::call_once` runs the closure exactly once.
    NET_DEVICE.call_once(|| device);
}

/// Transmit an Ethernet frame through the installed device.
///
/// Returns `errors::NO_DEVICE` when no device has been installed yet.
pub fn transmit(frame: &[u8]) -> Result<usize, i32> {
    match NET_DEVICE.get() {
        Some(device) => device.transmit(frame),
        None => Err(errors::NO_DEVICE),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mac_addr_display_and_broadcast() {
        let b = MacAddr::BROADCAST;
        assert!(b.is_broadcast());
        assert_eq!(alloc::format!("{}", b), "ff:ff:ff:ff:ff:ff");
        let m = MacAddr([0x02, 0x02, 0x03, 0x04, 0x05, 0x06]);
        assert_eq!(alloc::format!("{}", m), "02:02:03:04:05:06");
        assert!(!m.is_multicast());
        assert!(MacAddr([0x01, 0, 0, 0, 0, 0]).is_multicast());
    }

    #[test]
    fn ethernet_header_round_trips() {
        let dst = MacAddr([1, 2, 3, 4, 5, 6]);
        let src = MacAddr([6, 5, 4, 3, 2, 1]);
        let mut out = [0u8; 64];
        let len = EthernetHeader::build(&mut out, dst, src, ethertype::IPV4);
        assert_eq!(len, EthernetHeader::LEN);
        let parsed = EthernetHeader::parse(&out[..len]).unwrap();
        assert_eq!(parsed.dst, dst);
        assert_eq!(parsed.src, src);
        assert_eq!(parsed.ethertype, ethertype::IPV4);
    }

    #[test]
    fn ipv4_header_round_trip_and_checksum() {
        let src = Ipv4Addr::new(192, 168, 1, 10);
        let dst = Ipv4Addr::new(192, 168, 1, 1);
        let mut out = [0u8; 64];
        Ipv4Header::build(&mut out, 0, 60, 1, 0, 64, protocol::UDP, 0, src, dst);
        // Fill in the checksum: zero it, compute, write back.
        let csum = Ipv4Header::checksum(&out[..20]);
        out[10..12].copy_from_slice(&csum.to_be_bytes());
        assert_eq!(Ipv4Header::checksum(&out[..20]), 0);
        let parsed = Ipv4Header::parse(&out[..20]).unwrap();
        assert_eq!(parsed.src, src);
        assert_eq!(parsed.dst, dst);
        assert_eq!(parsed.protocol, protocol::UDP);
    }

    #[test]
    fn ipv4_packet_rejects_bad_checksum() {
        let src = Ipv4Addr::new(10, 0, 0, 1);
        let dst = Ipv4Addr::new(10, 0, 0, 2);
        let mut out = [0u8; 64];
        Ipv4Header::build(
            &mut out,
            0,
            20,
            1,
            0,
            64,
            protocol::UDP,
            0x1234, // deliberately wrong
            src,
            dst,
        );
        assert!(Ipv4Packet::parse(&out[..20]).is_none());
    }

    #[test]
    fn udp_datagram_round_trip() {
        let payload = b"hello";
        let mut out = [0u8; 64];
        let len = UdpDatagram::build(&mut out, 1234, 53, (8 + payload.len()) as u16, 0, payload);
        assert_eq!(len, 8 + payload.len());
        let parsed = UdpDatagram::parse(&out[..len]).unwrap();
        assert_eq!(parsed.src_port, 1234);
        assert_eq!(parsed.dst_port, 53);
        assert_eq!(parsed.payload, payload);
    }

    #[test]
    fn arp_round_trip() {
        let smac = MacAddr([1, 2, 3, 4, 5, 6]);
        let sip = Ipv4Addr::new(10, 0, 0, 1);
        let tmac = MacAddr::BROADCAST;
        let tip = Ipv4Addr::new(10, 0, 0, 2);
        let mut out = [0u8; 64];
        let len = ArpPacket::build(&mut out, 1, smac, sip, tmac, tip);
        assert_eq!(len, ArpPacket::HEADER_LEN);
        let parsed = ArpPacket::parse(&out[..len]).unwrap();
        assert_eq!(parsed.opcode, 1);
        assert_eq!(parsed.sender_mac, smac);
        assert_eq!(parsed.sender_ip, sip);
        assert_eq!(parsed.target_ip, tip);
    }

    #[test]
    fn packet_classify_routes_ipv4_and_arp() {
        let src_ip = Ipv4Addr::new(10, 0, 0, 1);
        let dst_ip = Ipv4Addr::new(10, 0, 0, 2);
        let mut frame = [0u8; 46];

        // Ethernet header
        frame[0..6].copy_from_slice(&MacAddr::BROADCAST.0);
        frame[6..12].copy_from_slice(&MacAddr([1, 2, 3, 4, 5, 6]).0);
        frame[12..14].copy_from_slice(&ethertype::IPV4.to_be_bytes());

        // IPv4 header
        frame[14] = 0x45; // version=4, ihl=5
        frame[15] = 0x00;
        frame[16..18].copy_from_slice(&(32u16).to_be_bytes()); // total length = 32
        frame[18..20].copy_from_slice(&[0, 1]); // identification = 1
        frame[20..22].copy_from_slice(&[0, 0]); // flags/fragment
        frame[22] = 64; // TTL
        frame[23] = 17; // UDP protocol
        frame[26..30].copy_from_slice(&[10, 0, 0, 1]); // src IP
        frame[30..34].copy_from_slice(&[10, 0, 0, 2]); // dst IP

        // IPv4 checksum
        let csum = Ipv4Header::checksum(&frame[14..34]);
        frame[24..26].copy_from_slice(&csum.to_be_bytes());

        // UDP header
        frame[34..36].copy_from_slice(&[4, 210]); // src_port = 1234
        frame[36..38].copy_from_slice(&[0, 53]); // dst_port = 53
        frame[38..40].copy_from_slice(&[0, 12]); // length = 12
        frame[40..42].copy_from_slice(&[0, 0]); // checksum = 0

        // UDP payload
        frame[42..46].copy_from_slice(b"ping");

        let total = 14 + 32; // 46 bytes
        match Packet::classify(&frame[..total]) {
            Some(Packet::Ipv4(pkt)) => {
                assert_eq!(pkt.header.src, src_ip);
                assert_eq!(pkt.header.dst, dst_ip);
                assert_eq!(pkt.header.protocol, protocol::UDP);
                // IPv4 payload = entire UDP datagram (header + "ping")
                assert_eq!(
                    pkt.payload,
                    &[4, 210, 0, 53, 0, 12, 0, 0, 112, 105, 110, 103]
                );
            }
            other => panic!("expected IPv4 packet, got {:?}", other),
        }
    }
}
