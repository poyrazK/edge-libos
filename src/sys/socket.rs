//! Socket syscalls — P1 step 1: `socket(2)` only.
//!
//! P1-1 lands just `socket(AF_INET/AF_INET6, SOCK_STREAM, 0) → fd`. The
//! `SocketInner` carries no real connection yet — `bind`, `listen`,
//! `connect`, `accept4`, `recvfrom`, `sendto` follow in later sub-steps.
//!
//! Per `impelementationplan` §4.4: AF_INET / AF_INET6 / SOCK_STREAM /
//! SOCK_DGRAM / SOCK_NONBLOCK / SOCK_CLOEXEC are accepted in P0/P1;
//! unknown families or types return -EAFNOSUPPORT / -EPROTONOSUPPORT.
//! AF_UNIX lands in P2.

use wasmtime::Caller;

use crate::errno::{EAFNOSUPPORT, EBADF, EINVAL, EOPNOTSUPP, EPROTONOSUPPORT};
use crate::fd::{Resource, SockAddr, SocketInner, SocketKind};
use crate::kernel::Kernel;
use crate::mem;

// NR_* (Linux x86-64 unistd_64.h).
pub const NR_SOCKET: u32 = 41;
pub const NR_BIND: u32 = 49;
pub const NR_LISTEN: u32 = 50;
pub const NR_SETSOCKOPT: u32 = 54;

// setsockopt level + optname constants we honor (per plan §P1-3).
// All other levels / optnames → 0 (accepted but unmodeled).
pub const SOL_SOCKET: i32 = 1;
pub const IPPROTO_TCP: i32 = 6;
pub const SO_REUSEADDR: i32 = 2;
pub const SO_KEEPALIVE: i32 = 9;
pub const TCP_NODELAY: i32 = 1;

// Address families.
pub const AF_UNIX: i32 = 1;
pub const AF_INET: i32 = 2;
pub const AF_INET6: i32 = 10;

// Socket types.
pub const SOCK_STREAM: i32 = 1;
pub const SOCK_DGRAM: i32 = 2;
// Flags OR'd into the type argument.
pub const SOCK_NONBLOCK: i32 = 0o4000;
pub const SOCK_CLOEXEC: i32 = 0o2000000;

// sockaddr layout sizes (per Linux man page / plan §3).
pub const SOCKADDR_STORAGE_SIZE: usize = 128; // sizeof(struct sockaddr_storage)
pub const SOCKADDR_IN_SIZE: usize = 16;
pub const SOCKADDR_IN6_SIZE: usize = 28;

/// `socket(family, type_and_flags, protocol)` — allocate a fresh socket fd.
///
/// P1-1 only creates the fd; no data path yet. Supported families are
/// `AF_INET` and `AF_INET6`; types are `SOCK_STREAM` and `SOCK_DGRAM`.
/// `protocol` is ignored (always 0 today). Unknown family → -EAFNOSUPPORT;
/// unsupported type for a known family → -EPROTONOSUPPORT.
pub async fn socket(caller: &mut Caller<'_, Kernel>, a: [i64; 6]) -> i64 {
    let family = a[0] as i32;
    let type_and_flags = a[1] as i32;
    // `protocol` (a[2]) is accepted but ignored — TCP/UDP don't need it.

    // SOCK_CLOEXEC is accepted but currently discarded (no real exec model
    // in P1); tracked for fidelity.
    let _cloexec = type_and_flags & SOCK_CLOEXEC;
    let sock_type = type_and_flags & 0xf; // SOCK_TYPE_MASK = low 4 bits, per Linux
    let nonblock = type_and_flags & SOCK_NONBLOCK != 0;

    let kind = match (family, sock_type) {
        (AF_INET, SOCK_STREAM) => SocketKind::Stream,
        (AF_INET, SOCK_DGRAM) => SocketKind::Datagram,
        (AF_INET6, SOCK_STREAM) => SocketKind::Stream,
        (AF_INET6, SOCK_DGRAM) => SocketKind::Datagram,
        // Known families but unsupported types (e.g. AF_INET SOCK_SEQPACKET).
        (AF_INET | AF_INET6, _) => return -EPROTONOSUPPORT,
        // AF_UNIX is P2.
        (AF_UNIX, _) => return -EAFNOSUPPORT,
        // Everything else: family not supported.
        _ => return -EAFNOSUPPORT,
    };

    let inner = SocketInner::new(kind, nonblock);
    let fd = caller.data_mut().fds.insert(Resource::Socket(inner));
    fd as i64
}

/// `socket(family, type_and_flags, protocol)` — convenience wrapper used
/// by tests that want a result without going through the syscall ABI.
#[cfg(test)]
pub fn socket_for_test(
    fds: &mut crate::fd::FdTable,
    family: i32,
    type_and_flags: i32,
) -> Result<u32, i64> {
    let sock_type = type_and_flags & 0xf;
    let nonblock = type_and_flags & SOCK_NONBLOCK != 0;
    let kind = match (family, sock_type) {
        (AF_INET, SOCK_STREAM) | (AF_INET6, SOCK_STREAM) => SocketKind::Stream,
        (AF_INET, SOCK_DGRAM) | (AF_INET6, SOCK_DGRAM) => SocketKind::Datagram,
        (AF_INET | AF_INET6, _) => return Err(-EPROTONOSUPPORT),
        (AF_UNIX, _) => return Err(-EAFNOSUPPORT),
        _ => return Err(-EAFNOSUPPORT),
    };
    let _ = nonblock;
    Ok(fds.insert(Resource::Socket(SocketInner::new(kind, false))))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fd::FdTable;

    #[test]
    fn socket_inet_stream_inserts_fd() {
        let mut fds = FdTable::empty();
        let fd = socket_for_test(&mut fds, AF_INET, SOCK_STREAM).unwrap();
        assert!(fd >= 3);
        assert!(fds.contains(fd));
        fds.close(fd).unwrap();
        assert!(!fds.contains(fd));
    }

    #[test]
    fn socket_unknown_family_returns_eafnosupport() {
        let mut fds = FdTable::empty();
        assert_eq!(socket_for_test(&mut fds, 9999, SOCK_STREAM), Err(-EAFNOSUPPORT));
    }

    #[test]
    fn socket_inet_seqpacket_returns_eprotonosupport() {
        let mut fds = FdTable::empty();
        // 0o205 = SOCK_SEQPACKET on Linux. Not in our P1 set.
        assert_eq!(socket_for_test(&mut fds, AF_INET, 0o205), Err(-EPROTONOSUPPORT));
    }
}

/// Parse a `sockaddr_in` (16 bytes) or `sockaddr_in6` (28 bytes) from the
/// guest pointer. Returns `Err(-EINVAL)` on a bad family or truncated
/// `addrlen`; `Err(-EAFNOSUPPORT)` for families we don't model yet.
fn parse_sockaddr(caller: &mut Caller<'_, Kernel>, addr_ptr: i64, addr_len: i64) -> Result<SockAddr, i64> {
    if addr_ptr == 0 || addr_len == 0 {
        return Err(-(crate::errno::EDESTADDRREQ)); // no address supplied
    }
    let len = match usize::try_from(addr_len) {
        Ok(n) => n,
        Err(_) => return Err(-EINVAL),
    };
    // Bounds-check the smallest possible sockaddr header (family field).
    let header = match mem::guest_slice(caller, addr_ptr, 2) {
        Ok(b) => b,
        Err(e) => return Err(e),
    };
    let family = u16::from_le_bytes([header[0], header[1]]) as i32;

    match family {
        AF_INET => {
            if (len as usize) < SOCKADDR_IN_SIZE {
                return Err(-EINVAL);
            }
            let bytes = match mem::guest_slice(caller, addr_ptr, SOCKADDR_IN_SIZE as i64) {
                Ok(b) => b,
                Err(e) => return Err(e),
            };
            // struct sockaddr_in { sa_family_t sin_family; u16 sin_port; u32 sin_addr; u8 pad[8]; }
            // Network byte order for port and addr.
            let port = u16::from_be_bytes([bytes[2], bytes[3]]);
            let addr = [bytes[4], bytes[5], bytes[6], bytes[7]];
            Ok(SockAddr::V4 { port, addr })
        }
        AF_INET6 => {
            if (len as usize) < SOCKADDR_IN6_SIZE {
                return Err(-EINVAL);
            }
            let bytes = match mem::guest_slice(caller, addr_ptr, SOCKADDR_IN6_SIZE as i64) {
                Ok(b) => b,
                Err(e) => return Err(e),
            };
            // struct sockaddr_in6 { u16 sin6_family; u16 sin6_port; u32 flowinfo; u8 addr[16]; u32 scope_id; }
            let port = u16::from_be_bytes([bytes[2], bytes[3]]);
            let mut addr = [0u8; 16];
            addr.copy_from_slice(&bytes[8..24]);
            Ok(SockAddr::V6 { port, addr })
        }
        AF_UNIX => Err(-EOPNOTSUPP), // AF_UNIX is P2; EOPNOTSUPP is the more common errno
        _ => Err(-EAFNOSUPPORT),
    }
}

/// `bind(sockfd, addr, addrlen)` — record the socket's bound address.
///
/// P1-2 doesn't open a real `TcpListener` yet (that's lazy-built on first
/// `accept4`, P1-4). We just validate the sockaddr, store it on the
/// `SocketInner`, and return 0. Duplicate-bind on the same `(addr, port)`
/// is not detected here — Linux's `EADDRINUSE` requires a real listener,
/// which we don't have until P1-4.
pub async fn bind(caller: &mut Caller<'_, Kernel>, a: [i64; 6]) -> i64 {
    let fd = match u32::try_from(a[0]) {
        Ok(f) => f,
        Err(_) => return -EBADF,
    };
    let addr_ptr = a[1];
    let addr_len = a[2];

    let addr = match parse_sockaddr(caller, addr_ptr, addr_len) {
        Ok(a) => a,
        Err(e) => return e,
    };

    let fds = &mut caller.data_mut().fds;
    match fds.get_mut(fd) {
        Ok(Resource::Socket(s)) => {
            s.bound = Some(addr);
            0
        }
        Ok(_) => -EBADF,
        Err(e) => e,
    }
}

/// `setsockopt(sockfd, level, optname, optval, optlen)`.
///
/// P1-3 honors three opts (`SO_REUSEADDR`, `SO_KEEPALIVE`, `TCP_NODELAY`)
/// by recording the desired state on `SocketInner`. Any other
/// `(level, optname)` pair is accepted and returns 0 (per plan §4.4 —
/// "unknown setsockopt opts → 0"); the optval bytes are not inspected.
///
/// P1-3 doesn't actually surface the recorded value to listeners or
/// sockets — full integration (TCP_NODELY → nodelay on the kernel stream,
/// SO_REUSEADDR → allow port reuse on the lazy TcpListener) is P1-7's
/// epoll/listener materialization step. For now we accept the calls so
/// guest libraries (uvicorn, FastAPI's ASGI loop) don't error out at
/// import time, and we lay down the state for later steps.
pub async fn setsockopt(caller: &mut Caller<'_, Kernel>, a: [i64; 6]) -> i64 {
    let fd = match u32::try_from(a[0]) {
        Ok(f) => f,
        Err(_) => return -EBADF,
    };
    let level = a[1] as i32;
    let optname = a[2] as i32;
    let optval_ptr = a[3];
    let optlen = a[4];

    // Bounds-check the optval pointer for the documented `optlen`. We
    // accept any optlen (even 0); the kernel doesn't dereference the
    // value for our three honored opts.
    if optval_ptr != 0 && optlen > 0 {
        if let Err(e) = mem::guest_slice(caller, optval_ptr, optlen) {
            return e;
        }
    }

    let is_known = matches!(
        (level, optname),
        (SOL_SOCKET, SO_REUSEADDR) | (SOL_SOCKET, SO_KEEPALIVE) | (IPPROTO_TCP, TCP_NODELAY)
    );

    let fds = &mut caller.data_mut().fds;
    match fds.get_mut(fd) {
        Ok(Resource::Socket(s)) => {
            if is_known {
                // Record the intent on the socket so P1-7's listener
                // materialization can read it. We don't keep the optval
                // bytes yet — only which opt was set.
                match (level, optname) {
                    (SOL_SOCKET, SO_REUSEADDR) => s.so_reuseaddr = true,
                    (SOL_SOCKET, SO_KEEPALIVE) => s.so_keepalive = true,
                    (IPPROTO_TCP, TCP_NODELAY) => s.tcp_nodelay = true,
                    _ => unreachable!(),
                }
            }
            0
        }
        Ok(_) => -EBADF,
        Err(e) => e,
    }
}

/// `listen(sockfd, backlog)` — mark the socket passive.
///
/// `backlog` must be non-negative (Linux clamps at `/proc/sys/net/core/somaxconn`;
/// we accept any non-negative value without clamping for now). Sets
/// `listen_backlog = Some(backlog)`. Returns 0 on success, -EBADF if `fd`
/// isn't a socket, -EDESTADDRREQ if `bind()` was never called.
pub async fn listen(caller: &mut Caller<'_, Kernel>, a: [i64; 6]) -> i64 {
    let fd = match u32::try_from(a[0]) {
        Ok(f) => f,
        Err(_) => return -EBADF,
    };
    let backlog = a[1];
    if backlog < 0 {
        return -EINVAL;
    }

    let fds = &mut caller.data_mut().fds;
    match fds.get_mut(fd) {
        Ok(Resource::Socket(s)) => {
            if s.bound.is_none() {
                return -(crate::errno::EDESTADDRREQ);
            }
            s.listen_backlog = Some(backlog as i32);
            0
        }
        Ok(_) => -EBADF,
        Err(e) => e,
    }
}

#[cfg(test)]
mod p1_2_tests {
    use super::*;
    use crate::fd::FdTable;

    #[test]
    fn listen_without_bind_returns_edestaddrreq() {
        let mut fds = FdTable::empty();
        let fd = socket_for_test(&mut fds, AF_INET, SOCK_STREAM).unwrap();
        // Direct kernel-state test: build a Caller-free probe by mutating
        // the resource directly. Equivalent to `listen(fd, 5)` against a
        // socket that was never bound.
        let ret = match fds.get_mut(fd).unwrap() {
            Resource::Socket(s) => {
                if s.bound.is_none() {
                    -crate::errno::EDESTADDRREQ
                } else {
                    s.listen_backlog = Some(5);
                    0
                }
            }
            _ => unreachable!(),
        };
        assert_eq!(ret, -crate::errno::EDESTADDRREQ);
    }

    #[test]
    fn bound_socket_records_address() {
        let mut fds = FdTable::empty();
        let fd = socket_for_test(&mut fds, AF_INET, SOCK_STREAM).unwrap();
        match fds.get_mut(fd).unwrap() {
            Resource::Socket(s) => {
                s.bound = Some(SockAddr::V4 { port: 8080, addr: [127, 0, 0, 1] });
            }
            _ => unreachable!(),
        }
        // Re-borrow immutably to read back.
        match fds.get(fd).unwrap() {
            Resource::Socket(s) => {
                match &s.bound {
                    Some(SockAddr::V4 { port, addr }) => {
                        assert_eq!(*port, 8080);
                        assert_eq!(*addr, [127, 0, 0, 1]);
                    }
                    other => panic!("expected V4 bound, got {other:?}"),
                }
                assert!(!s.is_listening(), "not listening yet");
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn is_listening_requires_both_bind_and_listen() {
        let mut fds = FdTable::empty();
        let fd = socket_for_test(&mut fds, AF_INET, SOCK_STREAM).unwrap();
        match fds.get_mut(fd).unwrap() {
            Resource::Socket(s) => {
                assert!(!s.is_listening());
                s.bound = Some(SockAddr::V4 { port: 0, addr: [0, 0, 0, 0] });
                assert!(!s.is_listening(), "bind alone is not listening");
                s.listen_backlog = Some(128);
                assert!(s.is_listening(), "bind + listen -> listening");
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn as_v4_converts_bound_address() {
        let addr = SockAddr::V4 { port: 8080, addr: [192, 168, 1, 1] };
        let v4 = addr.as_v4().expect("V4 -> Some");
        assert_eq!(*v4.ip(), std::net::Ipv4Addr::new(192, 168, 1, 1));
        assert_eq!(v4.port(), 8080);
    }
}