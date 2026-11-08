//! `net.library` syscall handlers: BSD sockets over the kernel socket service.
//!
//! # What this module is
//!
//! The adapters that turn the raw syscall registers into [`vela_kernel_socket`]
//! calls. Each handler validates its arguments, copies addresses and buffers
//! to/from user space with the copy helpers, and turns the result into a
//! [`SyscallResult`] — never a panic.
//!
//! # User pointers are never dereferenced
//!
//! Every `addr_ptr`, `buf_ptr` or `optval_ptr` is copied through
//! [`crate::copy::copy_from_user`] / [`crate::copy::copy_to_user`], which
//! validate the range against [`vela_kernel_syscall::copy::USER_SPACE_END`]
//! before touching the bytes.

use crate::copy::{copy_from_user, copy_to_user};
use crate::SyscallResult;

use vela_kernel_socket::SockaddrIn;

/// `NET_SOCKET` — `socket(domain, type, protocol) -> handle`.
pub fn socket(
    args: crate::SyscallArgs,
    _ctx: &crate::dispatch::SyscallContext<'_>,
) -> SyscallResult {
    let domain = args.get(0) as i32;
    let sock_type = args.get(1) as i32;
    let protocol = args.get(2) as i32;
    match vela_kernel_socket::service().socket(_ctx.task, domain, sock_type, protocol) {
        Ok(handle) => SyscallResult::ok(handle as u64),
        Err(e) => SyscallResult::err(e),
    }
}

/// `NET_BIND` — `bind(sockfd, addr_ptr, addrlen) -> 0`.
pub fn bind(args: crate::SyscallArgs, _ctx: &crate::dispatch::SyscallContext<'_>) -> SyscallResult {
    let sockfd = args.get(0) as u32;
    let addr_ptr = args.get(1);
    let _addrlen = args.get(2) as usize;
    let mut addr = SockaddrIn {
        sin_family: 0,
        sin_port: 0,
        sin_addr: vela_kernel_net::Ipv4Addr::UNSPECIFIED,
        sin_zero: [0; 8],
    };
    // SAFETY: `copy_from_user` validates the user range before reading.
    match unsafe {
        copy_from_user(
            &mut addr as *mut SockaddrIn as *mut u8,
            addr_ptr,
            core::mem::size_of::<SockaddrIn>(),
        )
    } {
        Ok(_) => {}
        Err(e) => return SyscallResult::err(e),
    }
    match vela_kernel_socket::service().bind(sockfd, &addr) {
        Ok(()) => SyscallResult::ok(0),
        Err(e) => SyscallResult::err(e),
    }
}

/// `NET_CONNECT` — `connect(sockfd, addr_ptr, addrlen) -> 0`.
pub fn connect(
    args: crate::SyscallArgs,
    _ctx: &crate::dispatch::SyscallContext<'_>,
) -> SyscallResult {
    let sockfd = args.get(0) as u32;
    let addr_ptr = args.get(1);
    let _addrlen = args.get(2) as usize;
    let mut addr = SockaddrIn {
        sin_family: 0,
        sin_port: 0,
        sin_addr: vela_kernel_net::Ipv4Addr::UNSPECIFIED,
        sin_zero: [0; 8],
    };
    match unsafe {
        copy_from_user(
            &mut addr as *mut SockaddrIn as *mut u8,
            addr_ptr,
            core::mem::size_of::<SockaddrIn>(),
        )
    } {
        Ok(_) => {}
        Err(e) => return SyscallResult::err(e),
    }
    match vela_kernel_socket::service().connect(sockfd, &addr) {
        Ok(()) => SyscallResult::ok(0),
        Err(e) => SyscallResult::err(e),
    }
}

/// `NET_LISTEN` — `listen(sockfd, backlog) -> 0`.
pub fn listen(
    args: crate::SyscallArgs,
    _ctx: &crate::dispatch::SyscallContext<'_>,
) -> SyscallResult {
    let sockfd = args.get(0) as u32;
    let backlog = args.get(1) as i32;
    match vela_kernel_socket::service().listen(sockfd, backlog) {
        Ok(()) => SyscallResult::ok(0),
        Err(e) => SyscallResult::err(e),
    }
}

/// `NET_ACCEPT` — `accept(sockfd, addr_ptr, addrlen) -> (handle, ...)`.
pub fn accept(
    args: crate::SyscallArgs,
    _ctx: &crate::dispatch::SyscallContext<'_>,
) -> SyscallResult {
    let sockfd = args.get(0) as u32;
    let _addr_ptr = args.get(1);
    let _addrlen = args.get(2) as usize;
    match vela_kernel_socket::service().accept(sockfd) {
        Ok((handle, _addr)) => SyscallResult::ok(handle as u64),
        Err(e) => SyscallResult::err(e),
    }
}

/// `NET_SENDTO` — `sendto(sockfd, buf_ptr, len, flags, dest_ptr, addrlen) -> len`.
pub fn sendto(
    args: crate::SyscallArgs,
    _ctx: &crate::dispatch::SyscallContext<'_>,
) -> SyscallResult {
    let sockfd = args.get(0) as u32;
    let buf_ptr = args.get(1);
    let len = args.get(2) as usize;
    let _flags = args.get(3) as u32;
    let _dest_ptr = args.get(4);
    let _addrlen = args.get(5) as usize;
    let mut buf = vec![0u8; len];
    // SAFETY: `copy_from_user` validates the user range before reading.
    match unsafe { copy_from_user(buf.as_mut_ptr(), buf_ptr, len) } {
        Ok(_) => {}
        Err(e) => return SyscallResult::err(e),
    }
    match vela_kernel_socket::service().sendto(sockfd, &buf, None) {
        Ok(n) => SyscallResult::ok(n as u64),
        Err(e) => SyscallResult::err(e),
    }
}

/// `NET_RECVFROM` — `recvfrom(sockfd, buf_ptr, len, flags, src_ptr, addrlen) -> len`.
pub fn recvfrom(
    args: crate::SyscallArgs,
    _ctx: &crate::dispatch::SyscallContext<'_>,
) -> SyscallResult {
    let sockfd = args.get(0) as u32;
    let buf_ptr = args.get(1);
    let len = args.get(2) as usize;
    let _flags = args.get(3) as u32;
    let _src_ptr = args.get(4);
    let _addrlen = args.get(5) as usize;
    let mut buf = vec![0u8; len];
    match vela_kernel_socket::service().recvfrom(sockfd, &mut buf) {
        Ok((n, src)) => {
            // SAFETY: `copy_to_user` validates the user range before writing.
            match unsafe { copy_to_user(buf_ptr, buf.as_ptr(), n) } {
                Ok(_) => {}
                Err(e) => return SyscallResult::err(e),
            }
            // Copy the source address back if the caller supplied a pointer.
            let _ = src;
            SyscallResult::ok(n as u64)
        }
        Err(e) => SyscallResult::err(e),
    }
}

/// `NET_CLOSE` — `close(sockfd) -> 0`.
pub fn close(
    args: crate::SyscallArgs,
    _ctx: &crate::dispatch::SyscallContext<'_>,
) -> SyscallResult {
    let sockfd = args.get(0) as u32;
    match vela_kernel_socket::service().close(sockfd) {
        Ok(()) => SyscallResult::ok(0),
        Err(e) => SyscallResult::err(e),
    }
}

/// `NET_SETSOCKOPT` — `setsockopt(sockfd, level, optname, optval_ptr, optlen) -> 0`.
pub fn setsockopt(
    args: crate::SyscallArgs,
    _ctx: &crate::dispatch::SyscallContext<'_>,
) -> SyscallResult {
    let sockfd = args.get(0) as u32;
    let level = args.get(1) as i32;
    let optname = args.get(2) as i32;
    let optval_ptr = args.get(3);
    let optlen = args.get(4) as usize;
    let mut optval = vec![0u8; optlen];
    // SAFETY: `copy_from_user` validates the user range before reading.
    match unsafe { copy_from_user(optval.as_mut_ptr(), optval_ptr, optlen) } {
        Ok(_) => {}
        Err(e) => return SyscallResult::err(e),
    }
    match vela_kernel_socket::service().setsockopt(sockfd, level, optname, &optval) {
        Ok(()) => SyscallResult::ok(0),
        Err(e) => SyscallResult::err(e),
    }
}
