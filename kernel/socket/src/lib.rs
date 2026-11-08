//! BSD socket API.
//!
//! # What this crate is for
//!
//! It implements the socket calls a user program expects — `socket`,
//! `bind`, `connect`, `listen`, `accept`, `sendto`, `recvfrom`, `close` and
//! `setsockopt` — on top of the network stack in [`vela-kernel-net`] and the
//! device framework in [`vela-kernel-device`].
//!
//! # Socket lifecycle
//!
//! ```text
//! socket()     → Allocated  (a socket handle, not yet bound)
//! bind()       → Bound      (assigned a local address)
//! connect()    → Connected  (for SOCK_STREAM; for SOCK_DGRAM just records
//!                              the default peer)
//! listen()     → Listening  (a server socket)
//! accept()     → Connected  (a newly accepted connection)
//! sendto()/recvfrom() → data transfer
//! close()      → Closed
//! ```
//!
//! # Why sockets are a global service for now
//!
//! Each socket carries the task that created it, but lookups are by handle
//! rather than by "the current task", matching the Exec/syscall convention
//! that a handle is an opaque capability. A future SMP version can make the
//! table per-CPU.

#![no_std]

extern crate alloc;

use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, Ordering};
use spin::Mutex;

use vela_kernel_exec::task::TaskId;
use vela_kernel_net::{Ipv4Addr, MacAddr};

// ========================================================================
// Address types
// ========================================================================

/// Socket address families.
pub mod domain {
    /// IPv4.
    pub const AF_INET: i32 = 2;
}

/// Socket types.
pub mod sock_type {
    /// Reliable, connection-oriented byte stream (TCP).
    pub const SOCK_STREAM: i32 = 1;
    /// Datagram (connectionless, unreliable) — UDP.
    pub const SOCK_DGRAM: i32 = 2;
}

/// IP protocols.
pub mod protocol {
    pub const IPPROTO_UDP: i32 = 17;
    pub const IPPROTO_TCP: i32 = 6;
}

/// Socket option levels.
pub mod opt_level {
    pub const SOL_SOCKET: i32 = 1;
}

/// Socket options.
pub mod opt_name {
    pub const SO_REUSEADDR: i32 = 2;
    pub const SO_BROADCAST: i32 = 5;
    pub const SO_RCVBUF: i32 = 8;
    pub const SO_SNDBUF: i32 = 7;
}

/// An IPv4 socket address, network byte order for the address/port fields.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SockaddrIn {
    pub sin_family: u16, // AF_INET
    pub sin_port: u16,   // network byte order
    pub sin_addr: Ipv4Addr,
    pub sin_zero: [u8; 8],
}

impl SockaddrIn {
    /// An IPv4 address plus port.
    pub const fn new(family: u16, port: u16, addr: Ipv4Addr) -> Self {
        Self {
            sin_family: family,
            sin_port: port,
            sin_addr: addr,
            sin_zero: [0; 8],
        }
    }

    /// The port in host byte order.
    pub const fn port(self) -> u16 {
        u16::from_be(self.sin_port)
    }

    /// True when this is a wildcard address (0.0.0.0).
    pub fn is_any(self) -> bool {
        self.sin_addr == Ipv4Addr::UNSPECIFIED
    }
}

/// Socket error codes (BSD errno values, negated as VelaOS errors).
pub mod errors {
    pub const OK: i32 = 0;
    pub const EBADF: i32 = -9; // bad file/socket descriptor
    pub const EINVAL: i32 = -22; // invalid argument
    pub const EAFNOSUPPORT: i32 = -97; // address family not supported
    pub const EOPNOTSUPP: i32 = -95; // operation not supported
    pub const EADDRINUSE: i32 = -98; // address already in use
    pub const EADDRNOTAVAIL: i32 = -99; // address not available
    pub const ECONNREFUSED: i32 = -111; // connection refused
    pub const ECONNRESET: i32 = -104; // connection reset
    pub const EISCONN: i32 = -106; // already connected
    pub const ENOTCONN: i32 = -107; // not connected
    pub const ENOBUFS: i32 = -105; // no buffer space
    pub const EMSGSIZE: i32 = -90; // message too long
    pub const EWOULDBLOCK: i32 = -11; // operation would block
    pub const EINPROGRESS: i32 = -115; // operation in progress
    pub const EDESTADDRREQ: i32 = -89; // destination address required
    pub const ENOPROTOOPT: i32 = -94; // protocol not available
}

// ========================================================================
// Socket
// ========================================================================

/// The state of a socket.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SocketState {
    Allocated,
    Bound,
    Connected,
    Listening,
    Closed,
}

/// One socket.
#[derive(Debug, Clone)]
pub struct Socket {
    /// The socket handle.
    pub handle: u32,
    /// The task that created the socket.
    pub task: TaskId,
    /// The address family.
    pub domain: i32,
    /// The socket type.
    pub sock_type: i32,
    /// The IP protocol.
    pub protocol: i32,
    /// The state.
    pub state: SocketState,
    /// The local address, if bound.
    pub local: Option<SockaddrIn>,
    /// The peer address, if connected.
    pub peer: Option<SockaddrIn>,
    /// Receive queue: pending datagrams (source address, payload).
    pub rx_queue: Vec<(SockaddrIn, alloc::vec::Vec<u8>)>,
    /// The next sequence handle for accept.
    pub next_conn: u32,
}

impl Socket {
    fn new(handle: u32, task: TaskId, domain: i32, sock_type: i32, protocol: i32) -> Self {
        Self {
            handle,
            task,
            domain,
            sock_type,
            protocol,
            state: SocketState::Allocated,
            local: None,
            peer: None,
            rx_queue: Vec::new(),
            next_conn: 1,
        }
    }
}

/// The socket service: the global socket table.
///
/// A socket handle indexes into this table. Handles are never reused while
/// the table is small, so a handle names one socket for the life of the
/// system.
pub struct SocketService {
    sockets: Mutex<BTreeMap<u32, Socket>>,
    next_handle: AtomicU32,
}

impl Default for SocketService {
    fn default() -> Self {
        Self::new()
    }
}

impl SocketService {
    /// An empty socket service.
    pub fn new() -> Self {
        Self {
            sockets: Mutex::new(BTreeMap::new()),
            next_handle: AtomicU32::new(1),
        }
    }

    /// `socket(domain, type, protocol)` — create a socket.
    pub fn socket(
        &self,
        task: TaskId,
        domain: i32,
        sock_type: i32,
        protocol: i32,
    ) -> Result<u32, i32> {
        if domain != domain::AF_INET {
            return Err(errors::EAFNOSUPPORT);
        }
        let is_stream = sock_type == sock_type::SOCK_STREAM;
        let is_dgram = sock_type == sock_type::SOCK_DGRAM;
        if !is_stream && !is_dgram {
            return Err(errors::EINVAL);
        }
        match protocol {
            protocol::IPPROTO_UDP if is_dgram => {}
            protocol::IPPROTO_TCP if is_stream => {}
            _ => return Err(errors::EINVAL),
        }
        let handle = self.next_handle.fetch_add(1, Ordering::Relaxed);
        let socket = Socket::new(handle, task, domain, sock_type, protocol);
        self.sockets.lock().insert(handle, socket);
        Ok(handle)
    }

    /// `bind(sockfd, addr, addrlen)` — assign a local address.
    pub fn bind(&self, sockfd: u32, addr: &SockaddrIn) -> Result<(), i32> {
        if addr.sin_family != domain::AF_INET as u16 {
            return Err(errors::EADDRNOTAVAIL);
        }
        // One lock for the whole operation: taking a second `lock()` while
        // the first is held would deadlock the service.
        let mut sockets = self.sockets.lock();
        let port = addr.sin_port;
        if sockets
            .values()
            .any(|s| matches!(s.local, Some(a) if a.sin_port == port && !a.is_any()))
        {
            return Err(errors::EADDRINUSE);
        }
        let socket = sockets.get_mut(&sockfd).ok_or(errors::EBADF)?;
        if socket.state != SocketState::Allocated {
            return Err(errors::EINVAL);
        }
        socket.local = Some(*addr);
        socket.state = SocketState::Bound;
        Ok(())
    }

    /// `connect(sockfd, addr, addrlen)` — associate with a peer.
    pub fn connect(&self, sockfd: u32, addr: &SockaddrIn) -> Result<(), i32> {
        let mut sockets = self.sockets.lock();
        let socket = sockets.get_mut(&sockfd).ok_or(errors::EBADF)?;
        if socket.sock_type == sock_type::SOCK_STREAM {
            return Err(errors::EOPNOTSUPP); // TCP not yet implemented
        }
        socket.peer = Some(*addr);
        socket.state = SocketState::Connected;
        Ok(())
    }

    /// `listen(sockfd, backlog)` — mark a socket as a passive acceptor.
    pub fn listen(&self, sockfd: u32, _backlog: i32) -> Result<(), i32> {
        let mut sockets = self.sockets.lock();
        let socket = sockets.get_mut(&sockfd).ok_or(errors::EBADF)?;
        if socket.sock_type != sock_type::SOCK_STREAM {
            return Err(errors::EOPNOTSUPP);
        }
        if socket.state != SocketState::Bound {
            return Err(errors::EINVAL);
        }
        socket.state = SocketState::Listening;
        Ok(())
    }

    /// `accept(sockfd, addr, addrlen)` — accept an incoming connection.
    pub fn accept(&self, sockfd: u32) -> Result<(u32, SockaddrIn), i32> {
        let mut sockets = self.sockets.lock();
        let socket = sockets.get_mut(&sockfd).ok_or(errors::EBADF)?;
        if socket.state != SocketState::Listening {
            return Err(errors::EINVAL);
        }
        // No TCP implementation yet: nothing to accept.
        let _ = socket.next_conn;
        Err(errors::EWOULDBLOCK)
    }

    /// `sendto(sockfd, buf, len, flags, dest, addrlen)` — send a datagram.
    pub fn sendto(&self, sockfd: u32, buf: &[u8], dest: Option<&SockaddrIn>) -> Result<usize, i32> {
        let mut sockets = self.sockets.lock();
        let socket = sockets.get_mut(&sockfd).ok_or(errors::EBADF)?;
        if socket.sock_type != sock_type::SOCK_DGRAM {
            return Err(errors::EOPNOTSUPP);
        }
        if socket.state != SocketState::Bound && socket.state != SocketState::Connected {
            return Err(errors::ENOTCONN);
        }
        let local = socket.local.ok_or(errors::EDESTADDRREQ)?;
        let dest = dest.or(socket.peer.as_ref()).ok_or(errors::EDESTADDRREQ)?;
        if buf.len() > u16::MAX as usize - 28 {
            // IP header (20) + UDP header (8) + Ethernet header (14).
            return Err(errors::EMSGSIZE);
        }
        // Build Ethernet + IPv4 + UDP frame into a stack buffer and transmit.
        let mut frame = [0u8; 1500];
        let eth_len = vela_kernel_net::EthernetHeader::build(
            &mut frame,
            MacAddr::BROADCAST,
            MacAddr::BROADCAST,
            vela_kernel_net::ethertype::IPV4,
        );
        let ip_total = (20 + 8 + buf.len()) as u16;
        let ip_len = vela_kernel_net::Ipv4Header::build(
            &mut frame[eth_len..],
            0,
            ip_total,
            1,
            0,
            64,
            vela_kernel_net::protocol::UDP,
            0,
            local.sin_addr,
            dest.sin_addr,
        );
        // IPv4 header checksum.
        let csum = vela_kernel_net::Ipv4Header::checksum(&frame[eth_len..eth_len + ip_len]);
        frame[eth_len + 10..eth_len + 12].copy_from_slice(&(!csum).to_be_bytes());
        let udp_len = (8 + buf.len()) as u16;
        let udp_end = eth_len + ip_len;
        let udp_len_total = vela_kernel_net::UdpDatagram::build(
            &mut frame[udp_end..],
            local.sin_port,
            dest.sin_port,
            udp_len,
            0,
            buf,
        );
        let _ = udp_len_total;
        let _ = ip_total;
        drop(sockets);
        // Transmit outside the lock so the driver can call back into the
        // stack without deadlocking.
        let len = vela_kernel_net::transmit(&frame[..eth_len + ip_len + udp_len as usize])?;
        // The device reports the whole frame. The socket reports the payload,
        // so subtract the Ethernet, IPv4 and UDP headers.
        Ok(len.saturating_sub(eth_len + ip_len + 8))
    }

    /// `recvfrom(sockfd, buf, len, flags, src, addrlen)` — receive a datagram.
    pub fn recvfrom(&self, sockfd: u32, buf: &mut [u8]) -> Result<(usize, SockaddrIn), i32> {
        let mut sockets = self.sockets.lock();
        let socket = sockets.get_mut(&sockfd).ok_or(errors::EBADF)?;
        if socket.sock_type != sock_type::SOCK_DGRAM {
            return Err(errors::EOPNOTSUPP);
        }
        match socket.rx_queue.pop() {
            Some((src, payload)) => {
                let n = payload.len().min(buf.len());
                buf[..n].copy_from_slice(&payload[..n]);
                Ok((n, src))
            }
            None => Err(errors::EWOULDBLOCK),
        }
    }

    /// `close(sockfd)` — release a socket.
    pub fn close(&self, sockfd: u32) -> Result<(), i32> {
        let mut sockets = self.sockets.lock();
        if sockets.remove(&sockfd).is_some() {
            Ok(())
        } else {
            Err(errors::EBADF)
        }
    }

    /// `setsockopt(sockfd, level, optname, optval, optlen)` — set an option.
    pub fn setsockopt(
        &self,
        sockfd: u32,
        level: i32,
        optname: i32,
        _optval: &[u8],
    ) -> Result<(), i32> {
        let mut sockets = self.sockets.lock();
        let socket = sockets.get_mut(&sockfd).ok_or(errors::EBADF)?;
        let _ = socket;
        if level != opt_level::SOL_SOCKET {
            return Err(errors::ENOPROTOOPT);
        }
        match optname {
            opt_name::SO_REUSEADDR
            | opt_name::SO_BROADCAST
            | opt_name::SO_RCVBUF
            | opt_name::SO_SNDBUF => Ok(()),
            _ => Err(errors::ENOPROTOOPT),
        }
    }

    /// Deliver a received UDP datagram to the bound socket on `port`.
    ///
    /// Called by the network stack's receive path when a datagram arrives.
    /// Returns `true` when a socket accepted the datagram.
    pub fn deliver_datagram(
        &self,
        dst_port: u16,
        src: SockaddrIn,
        payload: alloc::vec::Vec<u8>,
    ) -> bool {
        let mut sockets = self.sockets.lock();
        for socket in sockets.values_mut() {
            if socket.state == SocketState::Bound || socket.state == SocketState::Connected {
                if let Some(local) = socket.local {
                    if local.sin_port == dst_port {
                        socket.rx_queue.push((src, payload));
                        return true;
                    }
                } else if matches!(socket.peer, Some(p) if p.sin_port == dst_port) {
                    socket.rx_queue.push((src, payload));
                    return true;
                }
            }
        }
        false
    }

    /// How many sockets exist (for diagnostics).
    pub fn socket_count(&self) -> usize {
        self.sockets.lock().len()
    }
}

/// The global socket service.
///
/// Installed during boot in `kernel/entry`. Handlers reach it through this
/// static.
static SOCKET_SERVICE: spin::Once<SocketService> = spin::Once::new();

/// Get the global socket service, installing it on first use.
///
/// # Panics
///
/// Panics if called before the service has been installed by [`install`].
pub fn service() -> &'static SocketService {
    SOCKET_SERVICE
        .get()
        .expect("socket service not installed; kernel/entry must call socket::install")
}

/// Install the global socket service.
///
/// # Safety
///
/// Must be called exactly once during boot, before any socket syscall runs.
pub unsafe fn install(service: SocketService) {
    // `Once::call_once` runs the closure exactly once.
    SOCKET_SERVICE.call_once(|| service);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn service() -> SocketService {
        SocketService::new()
    }

    /// A transmit-only device for the socket test. The real boot path installs
    /// a hardware NIC; here the test needs an endpoint so `sendto` can succeed.
    struct FakeNet;

    impl vela_kernel_net::NetDevice for FakeNet {
        fn transmit(&self, frame: &[u8]) -> Result<usize, i32> {
            Ok(frame.len())
        }
    }

    static FAKE_NET: FakeNet = FakeNet;

    #[test]
    fn socket_bind_sendto_recvfrom_round_trip() {
        // SAFETY: the socket test installs the fake device once for its own
        // process before any transmit.
        unsafe { vela_kernel_net::install_net_device(&FAKE_NET) };
        let s = service();
        let task = vela_kernel_exec::task::TaskId(1);
        let handle = s
            .socket(
                task,
                domain::AF_INET,
                sock_type::SOCK_DGRAM,
                protocol::IPPROTO_UDP,
            )
            .unwrap();
        assert_ne!(handle, 0);

        let addr = SockaddrIn::new(domain::AF_INET as u16, 5000, Ipv4Addr::new(10, 0, 0, 1));
        assert!(s.bind(handle, &addr).is_ok());
        assert!(s.bind(handle, &addr).is_err()); // EADDRINUSE on rebind after bind

        // Connect to a peer (UDP "connect" just records the default peer).
        let peer = SockaddrIn::new(domain::AF_INET as u16, 5001, Ipv4Addr::new(10, 0, 0, 2));
        assert!(s.connect(handle, &peer).is_ok());

        // sendto without a destination uses the recorded peer.
        let sent = s.sendto(handle, b"hello", None).unwrap();
        assert_eq!(sent, 5);

        // Recv returns EWOULDBLOCK until a datagram is delivered.
        let mut buf = [0u8; 64];
        assert_eq!(
            s.recvfrom(handle, &mut buf).unwrap_err(),
            errors::EWOULDBLOCK
        );

        // Deliver a datagram and read it back.
        let src = SockaddrIn::new(domain::AF_INET as u16, 5001, Ipv4Addr::new(10, 0, 0, 2));
        assert!(s.deliver_datagram(5000, src, b"reply".to_vec()));
        let mut buf = [0u8; 64];
        let (n, src) = s.recvfrom(handle, &mut buf).unwrap();
        assert_eq!(n, 5);
        assert_eq!(&buf[..n], b"reply");
        assert_eq!(src.sin_port, 5001);

        assert!(s.close(handle).is_ok());
        assert_eq!(s.close(handle).unwrap_err(), errors::EBADF);
    }

    #[test]
    fn socket_rejects_bad_domain_and_type() {
        let s = service();
        let task = vela_kernel_exec::task::TaskId(1);
        assert_eq!(
            s.socket(task, vela_kernel_net::ethertype::IPV4 as i32, 1, 0)
                .unwrap_err(),
            errors::EAFNOSUPPORT
        );
        assert_eq!(
            s.socket(
                task,
                domain::AF_INET,
                sock_type::SOCK_STREAM,
                protocol::IPPROTO_UDP
            )
            .unwrap_err(),
            errors::EINVAL
        );
    }

    #[test]
    fn setsockopt_accepts_known_socket_options() {
        let s = service();
        let task = vela_kernel_exec::task::TaskId(1);
        let handle = s
            .socket(
                task,
                domain::AF_INET,
                sock_type::SOCK_DGRAM,
                protocol::IPPROTO_UDP,
            )
            .unwrap();
        assert!(s
            .setsockopt(handle, opt_level::SOL_SOCKET, opt_name::SO_REUSEADDR, &[1])
            .is_ok());
        assert!(s
            .setsockopt(handle, opt_level::SOL_SOCKET, 999, &[])
            .is_err());
        assert!(s.close(handle).is_ok());
    }
}
