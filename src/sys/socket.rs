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

use crate::errno::{EAFNOSUPPORT, EBADF, EFAULT, EINVAL, EOPNOTSUPP, EPROTONOSUPPORT};
use crate::fd::{Resource, SockAddr, SocketInner, SocketKind};
use crate::kernel::Kernel;
use crate::mem;

// NR_* (Linux x86-64 unistd_64.h).
pub const NR_SOCKET: u32 = 41;
pub const NR_BIND: u32 = 49;
pub const NR_LISTEN: u32 = 50;
pub const NR_SETSOCKOPT: u32 = 54;
pub const NR_GETSOCKOPT: u32 = 55;
pub const NR_GETSOCKNAME: u32 = 51;
pub const NR_GETPEERNAME: u32 = 52;
pub const NR_SHUTDOWN: u32 = 48;
pub const NR_ACCEPT: u32 = 43;
pub const NR_ACCEPT4: u32 = 288;
pub const NR_CONNECT: u32 = 42;
pub const NR_SENDTO: u32 = 44;
pub const NR_RECVFROM: u32 = 45;

// setsockopt level + optname constants we honor (per plan §P1-3).
// All other levels / optnames → 0 (accepted but unmodeled).
pub const SOL_SOCKET: i32 = 1;
pub const IPPROTO_TCP: i32 = 6;
pub const SO_REUSEADDR: i32 = 2;
pub const SO_KEEPALIVE: i32 = 9;
pub const SO_TYPE: i32 = 3;
pub const SO_ERROR: i32 = 4;
pub const SO_DOMAIN: i32 = 39; // Linux-specific; musl defines it
pub const SO_ACCEPTCONN: i32 = 30;
pub const TCP_NODELAY: i32 = 1;

// shutdown(2) `how` values.
pub const SHUT_RD: i32 = 0;
pub const SHUT_WR: i32 = 1;
pub const SHUT_RDWR: i32 = 2;

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

/// `accept4(sockfd, addr, addrlen_ptr, flags)` — accept a connection on
/// a listening socket. Returns a new fd whose `SocketInner.stream` holds
/// the connected `TcpStream`. The first async-suspending socket syscall.
///
/// `flags`:
/// * `SOCK_NONBLOCK` — propagates to the new fd's `nonblock` bit.
/// * `SOCK_CLOEXEC` — accepted, recorded for fidelity (P1 doesn't model
///   exec, but the value is honored if a future exec path is added).
///
/// Errors:
/// - `-EBADF`   — fd not a Socket.
/// - `-EINVAL`  — not listening (bind + listen required).
/// - `-EFAULT`  — addr pointer out of bounds (only if addr != NULL).
/// - `-EOPNOTSUPP` — bound address is not IPv4 (V6 listener is P1-7).
pub async fn accept4(caller: &mut Caller<'_, Kernel>, a: [i64; 6]) -> i64 {
    let fd = match u32::try_from(a[0]) {
        Ok(f) => f,
        Err(_) => return -EBADF,
    };
    let addr_ptr = a[1];
    let addrlen_ptr = a[2];
    let flags = a[3] as i32;

    let sock_nonblock = flags & SOCK_NONBLOCK != 0;
    let _sock_cloexec = flags & SOCK_CLOEXEC;

    // Validate output pointers up front. addr gets 16 bytes (sockaddr_in);
    // addrlen gets 4 bytes (socklen_t). EFAULT on failure.
    if addr_ptr != 0 {
        if let Err(e) = mem::guest_slice_mut(caller, addr_ptr, 16) {
            return e;
        }
    }
    if addrlen_ptr != 0 {
        if let Err(e) = mem::guest_slice_mut(caller, addrlen_ptr, 4) {
            return e;
        }
    }

    // Phase 1: pull the (possibly-lazy) listener out of the resource.
    // We hold it as a local Option so we can `.await` outside the
    // `&mut Caller` borrow.
    let listener_opt: Option<tokio::net::TcpListener>;
    let kind;
    let parent_nonblock: bool;
    {
        let fds = &mut caller.data_mut().fds;
        match fds.get_mut(fd) {
            Ok(Resource::Socket(s)) => {
                if !s.is_listening() {
                    return -EINVAL;
                }
                if s.listener.is_none() {
                    let addr = match &s.bound {
                        Some(SockAddr::V4 { port, addr }) => std::net::SocketAddrV4::new(
                            std::net::Ipv4Addr::from(*addr),
                            *port,
                        ),
                        Some(SockAddr::V6 { .. }) => return -EOPNOTSUPP,
                        None => return -EINVAL,
                    };
                    let listener = match tokio::net::TcpListener::bind(addr).await {
                        Ok(l) => l,
                        Err(_) => return -crate::errno::EADDRINUSE,
                    };
                    s.listener = Some(listener);
                    // Mark this socket as a listening acceptor — surfaces in
                    // getsockopt(SO_ACCEPTCONN).
                    s.is_acceptor = true;
                }
                kind = s.kind;
                parent_nonblock = s.nonblock.load(std::sync::atomic::Ordering::Relaxed);
                listener_opt = s.listener.take();
            }
            Ok(_) => return -EBADF,
            Err(e) => return e,
        }
    }

    let listener = match listener_opt {
        Some(l) => l,
        None => return -EINVAL,
    };

    // Phase 2: await a connection. This may suspend.
    let (stream, peer) = match listener.accept().await {
        Ok(pair) => pair,
        Err(_) => return -crate::errno::EIO,
    };

    // Phase 3: write back the peer sockaddr if requested.
    if addr_ptr != 0 {
        if let Ok(buf) = mem::guest_slice_mut(caller, addr_ptr, 16) {
            match peer {
                std::net::SocketAddr::V4(v4) => {
                    let ip = v4.ip().octets();
                    let port = v4.port();
                    buf[0..2].copy_from_slice(&(AF_INET as u16).to_le_bytes());
                    buf[2..4].copy_from_slice(&port.to_be_bytes());
                    buf[4..8].copy_from_slice(&ip);
                    for b in &mut buf[8..16] {
                        *b = 0;
                    }
                }
                std::net::SocketAddr::V6(_) => return -EOPNOTSUPP,
            }
        }
    }
    if addrlen_ptr != 0 {
        if let Ok(buf) = mem::guest_slice_mut(caller, addrlen_ptr, 4) {
            buf[0..4].copy_from_slice(&(16u32).to_le_bytes());
        }
    }

    // Phase 4: put the listener back so subsequent accepts work.
    {
        let fds = &mut caller.data_mut().fds;
        if let Ok(Resource::Socket(s)) = fds.get_mut(fd) {
            if s.listener.is_none() {
                s.listener = Some(listener);
            }
            // If a concurrent caller already restored one, drop ours.
        }
    }

    // Phase 5: insert the accepted stream as a fresh fd.
    let mut accepted = SocketInner::from_accepted(stream, kind, sock_nonblock || parent_nonblock);
    // Record the peer address for `getpeername`.
    accepted.peer_addr = Some(peer);
    let new_fd = caller.data_mut().fds.insert(Resource::Socket(accepted));
    new_fd as i64
}

/// `accept(fd, addr, addrlen)` — legacy syscall. Implemented as a
/// shim over `accept4(fd, addr, addrlen, 0)` (no flags).
pub async fn accept(caller: &mut Caller<'_, Kernel>, a: [i64; 6]) -> i64 {
    accept4(caller, [a[0], a[1], a[2], 0, 0, 0]).await
}

/// `connect(sockfd, addr, addrlen)` — connect a Socket to a peer.
///
/// P1-5 supports AF_INET + SOCK_STREAM only. UDP connect (for a future
/// sendto path) is not modeled. The resulting `TcpStream` is stored on
/// `SocketInner.stream` and reused by `sendto`/`recvfrom`.
///
/// Errors:
/// - `-EBADF`     — fd not a Socket.
/// - `-EINVAL`    — already connected, or addrlen out of range.
/// - `-EAFNOSUPPORT` — bad family / not IPv4.
/// - `-EISCONN`   — fd already has a stream.
/// - `-ECONNREFUSED` / `-ENETUNREACH` — tokio's `TcpStream::connect`
///   surfaces host errors as their Linux errno equivalents.
pub async fn connect(caller: &mut Caller<'_, Kernel>, a: [i64; 6]) -> i64 {
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

    let target = match &addr {
        SockAddr::V4 { port, addr: bytes } => std::net::SocketAddr::V4(
            std::net::SocketAddrV4::new(std::net::Ipv4Addr::from(*bytes), *port),
        ),
        SockAddr::V6 { .. } => return -EOPNOTSUPP,
    };

    // Pre-check: is this a Socket? Does it already have a stream?
    {
        let fds = &caller.data().fds;
        match fds.get(fd) {
            Ok(Resource::Socket(s)) => {
                if s.stream.is_some() {
                    return -crate::errno::EISCONN;
                }
            }
            Ok(_) => return -EBADF,
            Err(e) => return e,
        }
    }

    let stream = match tokio::net::TcpStream::connect(target).await {
        Ok(s) => s,
        Err(e) => {
            // Map a handful of common tokio errors to Linux errnos.
            use std::io::ErrorKind::*;
            let errno = match e.kind() {
                ConnectionRefused => crate::errno::ECONNREFUSED,
                AddrInUse => crate::errno::EADDRINUSE,
                TimedOut => crate::errno::ETIMEDOUT,
                _ => crate::errno::EIO,
            };
            // Record the error on the socket for getsockopt(SO_ERROR).
            // P1-6: getsockopt will surface this and clear it.
            if let Ok(Resource::Socket(s)) = caller.data().fds.get(fd) {
                s.last_error
                    .store(errno as i32, std::sync::atomic::Ordering::Relaxed);
            }
            return -errno;
        }
    };

    let fds = &mut caller.data_mut().fds;
    match fds.get_mut(fd) {
        Ok(Resource::Socket(s)) => {
            s.stream = Some(stream);
            // Record peer for getpeername — we know the target.
            s.peer_addr = Some(target);
            0
        }
        Ok(_) => -EBADF,
        Err(e) => e,
    }
}

/// `sendto(fd, buf, len, flags, addr, addrlen)` — write `len` bytes from
/// the guest's `buf` to the connected TcpStream. `addr` and `addrlen`
/// are accepted but ignored for TCP (the connection's peer is fixed).
///
/// P1-5 doesn't honor MSG_DONTWAIT/flags — they would only matter for
/// O_NONBLOCK dispatch, which P1-3 doesn't actually integrate with send
/// paths yet. The byte transfer is async-suspending.
///
/// Errors:
/// - `-EBADF`     — fd not a Socket, or no connected stream.
/// - `-EFAULT`    — buf pointer out of bounds.
/// - `-ENOTCONN`  — stream is `None`.
pub async fn sendto(caller: &mut Caller<'_, Kernel>, a: [i64; 6]) -> i64 {
    let fd = match u32::try_from(a[0]) {
        Ok(f) => f,
        Err(_) => return -EBADF,
    };
    let buf_ptr = a[1];
    let buf_len_raw = a[2];
    let _flags = a[3] as i32; // ignored in P1-5
    let _addr_ptr = a[4];    // ignored for TCP
    let _addrlen = a[5];

    let len = match usize::try_from(buf_len_raw) {
        Ok(n) => n,
        Err(_) => return -EFAULT,
    };

    let bytes = match mem::guest_slice(caller, buf_ptr, buf_len_raw) {
        Ok(b) => b.to_vec(),
        Err(e) => return e,
    };

    // Pull the stream out so we can `.await` outside the &mut caller borrow.
    let mut stream = {
        let fds = &mut caller.data_mut().fds;
        match fds.get_mut(fd) {
            Ok(Resource::Socket(s)) => {
                // P1-6: SHUT_WR was called — writes must fail with EPIPE.
                if s.shutdown_flags & 0b10 != 0 {
                    return -crate::errno::EPIPE;
                }
                match s.stream.take() {
                    Some(st) => st,
                    None => return -crate::errno::ENOTCONN,
                }
            }
            Ok(_) => return -EBADF,
            Err(e) => return e,
        }
    };

    use tokio::io::AsyncWriteExt;
    let n = match stream.write(&bytes).await {
        Ok(n) => n,
        Err(_) => {
            // Put the stream back before returning.
            let fds = &mut caller.data_mut().fds;
            if let Ok(Resource::Socket(s)) = fds.get_mut(fd) {
                if s.stream.is_none() {
                    s.stream = Some(stream);
                }
            }
            return -crate::errno::EIO;
        }
    };

    // Put the stream back.
    {
        let fds = &mut caller.data_mut().fds;
        if let Ok(Resource::Socket(s)) = fds.get_mut(fd) {
            if s.stream.is_none() {
                s.stream = Some(stream);
            }
        }
    }

    let _ = len; // length is implicit in the write result
    n as i64
}

/// `recvfrom(fd, buf, len, flags, addr, addrlen)` — read up to `len`
/// bytes from the connected TcpStream into the guest's `buf`. The peer's
/// `sockaddr` is written back to `addr` if the guest supplied one
/// (same shape as accept4's peer write-back).
///
/// Errors:
/// - `-EBADF`    — fd not a Socket, or no connected stream.
/// - `-EFAULT`   — buf pointer out of bounds.
/// - `-ENOTCONN` — stream is `None`.
/// - `-EINVAL`   — `len` is 0.
pub async fn recvfrom(caller: &mut Caller<'_, Kernel>, a: [i64; 6]) -> i64 {
    let fd = match u32::try_from(a[0]) {
        Ok(f) => f,
        Err(_) => return -EBADF,
    };
    let buf_ptr = a[1];
    let buf_len_raw = a[2];
    let _flags = a[3] as i32;
    let addr_ptr = a[4];
    let addrlen_ptr = a[5];

    let len = match usize::try_from(buf_len_raw) {
        Ok(n) => n,
        Err(_) => return -EFAULT,
    };
    if len == 0 {
        return -EINVAL;
    }

    // Bounds-check the buf up front. recvfrom will overwrite the bytes.
    if let Err(e) = mem::guest_slice_mut(caller, buf_ptr, buf_len_raw) {
        return e;
    }
    if addr_ptr != 0 {
        if let Err(e) = mem::guest_slice_mut(caller, addr_ptr, 16) {
            return e;
        }
    }
    if addrlen_ptr != 0 {
        if let Err(e) = mem::guest_slice_mut(caller, addrlen_ptr, 4) {
            return e;
        }
    }

    // Pull the stream out so we can `.await` outside the &mut caller borrow.
    let mut stream = {
        let fds = &mut caller.data_mut().fds;
        match fds.get_mut(fd) {
            Ok(Resource::Socket(s)) => {
                // P1-6: SHUT_RD was called — reads return EOF immediately.
                if s.shutdown_flags & 0b01 != 0 {
                    return 0;
                }
                match s.stream.take() {
                    Some(st) => st,
                    None => return -crate::errno::ENOTCONN,
                }
            }
            Ok(_) => return -EBADF,
            Err(e) => return e,
        }
    };

    use tokio::io::AsyncReadExt;
    let mut buf = vec![0u8; len];
    let n = match stream.read(&mut buf).await {
        Ok(0) => 0, // EOF
        Ok(n) => n,
        Err(_) => {
            let fds = &mut caller.data_mut().fds;
            if let Ok(Resource::Socket(s)) = fds.get_mut(fd) {
                if s.stream.is_none() {
                    s.stream = Some(stream);
                }
            }
            return -crate::errno::EIO;
        }
    };

    // Put the stream back.
    {
        let fds = &mut caller.data_mut().fds;
        if let Ok(Resource::Socket(s)) = fds.get_mut(fd) {
            if s.stream.is_none() {
                s.stream = Some(stream);
            }
        }
    }

    // Copy bytes into guest buffer.
    let dst = match mem::guest_slice_mut(caller, buf_ptr, n as i64) {
        Ok(b) => b,
        Err(e) => return e,
    };
    dst[..n].copy_from_slice(&buf[..n]);

    // Best-effort peer sockaddr write-back (the peer is fixed for TCP).
    // We use 127.0.0.1:0 as a placeholder since tokio doesn't expose the
    // remote peer port cheaply. P1-6's getsockname/getpeername will fix
    // this with the actual peer.
    if addr_ptr != 0 {
        if let Ok(buf) = mem::guest_slice_mut(caller, addr_ptr, 16) {
            buf[0..2].copy_from_slice(&(AF_INET as u16).to_le_bytes());
            buf[2..4].copy_from_slice(&0u16.to_be_bytes()); // port unknown
            buf[4..8].copy_from_slice(&[127, 0, 0, 1]);
            for b in &mut buf[8..16] {
                *b = 0;
            }
        }
    }
    if addrlen_ptr != 0 {
        if let Ok(buf) = mem::guest_slice_mut(caller, addrlen_ptr, 4) {
            buf[0..4].copy_from_slice(&(16u32).to_le_bytes());
        }
    }

    n as i64
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

/// `getsockopt(fd, level, optname, optval, optlen_ptr)` — read a socket
/// option into the guest's `optval` buffer. Honors:
///   * `SO_TYPE`     → 1 (SOCK_STREAM) for stream sockets.
///   * `SO_DOMAIN`   → AF_INET (2).
///   * `SO_ERROR`    → reads `last_error` atomically and clears it.
///   * `SO_ACCEPTCONN` → 1 if the socket has a listener materialized.
///   * `SO_REUSEADDR` / `SO_KEEPALIVE` / `TCP_NODELAY` → 0/1 from the
///     recorded state on `SocketInner`.
///
/// All other `(level, optname)` pairs are accepted and write 0 — same
/// "unknown setsockopt opts → 0" contract as P1-3.
///
/// Errors: `-EBADF`, `-EFAULT` on bad optval/len pointers.
/// `getsockopt(fd, level, optname, optval, optlen_ptr)` — read a socket
/// option into the guest's `optval` buffer. Honors:
///   * `SO_TYPE`       → 1 (SOCK_STREAM) for stream sockets.
///   * `SO_DOMAIN`     → AF_INET (2).
///   * `SO_ERROR`      → reads `last_error` atomically and clears it.
///   * `SO_ACCEPTCONN` → 1 if the socket has a listener materialized.
///   * `SO_REUSEADDR` / `SO_KEEPALIVE` / `TCP_NODELAY` → 0/1 from the
///     recorded state on `SocketInner`.
///
/// All other `(level, optname)` pairs are accepted and write 0 — same
/// "unknown setsockopt opts → 0" contract as P1-3.
///
/// Errors: `-EBADF`, `-EFAULT` on bad optval/len pointers.
pub async fn getsockopt(caller: &mut Caller<'_, Kernel>, a: [i64; 6]) -> i64 {
    let fd = match u32::try_from(a[0]) {
        Ok(f) => f,
        Err(_) => return -EBADF,
    };
    let level = a[1] as i32;
    let optname = a[2] as i32;
    let optval_ptr = a[3];
    let optlen_ptr = a[4];

    if optval_ptr == 0 {
        return -crate::errno::EFAULT;
    }

    // Phase 1: compute the value to write. We do this while holding only
    // an immutable borrow on `caller` so the borrow checker is happy.
    // SO_ERROR swaps atomically, so we mutate here — that's fine because
    // it's `&self`-internal state on the SocketInner, not `caller`.
    let value: i32 = {
        let fds_table = &caller.data().fds;
        match (level, optname) {
            (SOL_SOCKET, SO_TYPE) => 1, // SOCK_STREAM (only stream modeled)
            (SOL_SOCKET, SO_DOMAIN) => 2, // AF_INET
            (SOL_SOCKET, SO_ERROR) => match fds_table.get(fd) {
                Ok(Resource::Socket(s)) => s
                    .last_error
                    .swap(0, std::sync::atomic::Ordering::Relaxed),
                _ => 0,
            }
            (SOL_SOCKET, SO_ACCEPTCONN) => match fds_table.get(fd) {
                Ok(Resource::Socket(s)) => s.is_acceptor as i32,
                _ => 0,
            },
            (SOL_SOCKET, SO_REUSEADDR) => match fds_table.get(fd) {
                Ok(Resource::Socket(s)) => s.so_reuseaddr as i32,
                _ => 0,
            },
            (SOL_SOCKET, SO_KEEPALIVE) => match fds_table.get(fd) {
                Ok(Resource::Socket(s)) => s.so_keepalive as i32,
                _ => 0,
            },
            (IPPROTO_TCP, TCP_NODELAY) => match fds_table.get(fd) {
                Ok(Resource::Socket(s)) => s.tcp_nodelay as i32,
                _ => 0,
            },
            _ => 0,
        }
    };

    // Phase 2: write the value into the guest optval buffer.
    {
        let buf = match mem::guest_slice_mut(caller, optval_ptr, 4) {
            Ok(b) => b,
            Err(e) => return e,
        };
        buf[0..4].copy_from_slice(&(value as u32).to_le_bytes());
    }

    // Phase 3: write back the optlen (size we actually wrote) if asked.
    if optlen_ptr != 0 {
        if let Ok(buf) = mem::guest_slice_mut(caller, optlen_ptr, 4) {
            buf[0..4].copy_from_slice(&(4u32).to_le_bytes());
        }
    }

    0
}
/// `getsockname(fd, addr_ptr, addrlen_ptr)` — write back the locally-bound
/// address (the `bound` field set by `bind()`). For accepted/connected
/// sockets, falls back to `peer_addr` so callers see something useful.
///
/// Errors: `-EBADF`, `-EFAULT` on bad pointers.
pub async fn getsockname(caller: &mut Caller<'_, Kernel>, a: [i64; 6]) -> i64 {
    let fd = match u32::try_from(a[0]) {
        Ok(f) => f,
        Err(_) => return -EBADF,
    };
    let addr_ptr = a[1];
    let addrlen_ptr = a[2];

    if addr_ptr == 0 {
        return -crate::errno::EFAULT;
    }
    // Pre-validate pointers.
    if let Err(e) = mem::guest_slice_mut(caller, addr_ptr, SOCKADDR_IN_SIZE as i64) {
        return e;
    }
    if addrlen_ptr != 0 {
        if let Err(e) = mem::guest_slice_mut(caller, addrlen_ptr, 4) {
            return e;
        }
    }

    // Snapshot the address we want to report.
    let local_addr: std::net::SocketAddr = {
        let fds = &caller.data().fds;
        match fds.get(fd) {
            Ok(Resource::Socket(s)) => match &s.bound {
                Some(crate::fd::SockAddr::V4 { port, addr }) => {
                    std::net::SocketAddr::V4(std::net::SocketAddrV4::new(
                        std::net::Ipv4Addr::from(*addr),
                        *port,
                    ))
                }
                Some(crate::fd::SockAddr::V6 { .. }) => {
                    // IPv6 support is P2; report EINVAL for now.
                    return -EINVAL;
                }
                None => match &s.peer_addr {
                    // No bind yet — fall back to the peer's family so
                    // getpeername callers can still read the response.
                    Some(p) => *p,
                    None => std::net::SocketAddr::V4(std::net::SocketAddrV4::new(
                        std::net::Ipv4Addr::new(0, 0, 0, 0),
                        0,
                    )),
                },
            },
            Ok(_) => return -EBADF,
            Err(e) => return e,
        }
    };

    if let Ok(buf) = mem::guest_slice_mut(caller, addr_ptr, SOCKADDR_IN_SIZE as i64) {
        match local_addr {
            std::net::SocketAddr::V4(v4) => {
                let ip = v4.ip().octets();
                let port = v4.port();
                buf[0..2].copy_from_slice(&(AF_INET as u16).to_le_bytes());
                buf[2..4].copy_from_slice(&port.to_be_bytes());
                buf[4..8].copy_from_slice(&ip);
                for b in &mut buf[8..16] {
                    *b = 0;
                }
            }
            std::net::SocketAddr::V6(_) => return -EINVAL,
        }
    }
    if addrlen_ptr != 0 {
        if let Ok(buf) = mem::guest_slice_mut(caller, addrlen_ptr, 4) {
            buf[0..4].copy_from_slice(&(16u32).to_le_bytes());
        }
    }
    0
}

/// `getpeername(fd, addr_ptr, addrlen_ptr)` — write back the remote peer
/// address (from `peer_addr`, set by `accept4` or `connect`).
///
/// Errors: `-EBADF`, `-ENOTCONN` if no peer is associated yet,
/// `-EFAULT` on bad pointers.
pub async fn getpeername(caller: &mut Caller<'_, Kernel>, a: [i64; 6]) -> i64 {
    let fd = match u32::try_from(a[0]) {
        Ok(f) => f,
        Err(_) => return -EBADF,
    };
    let addr_ptr = a[1];
    let addrlen_ptr = a[2];

    if addr_ptr == 0 {
        return -crate::errno::EFAULT;
    }
    if let Err(e) = mem::guest_slice_mut(caller, addr_ptr, SOCKADDR_IN_SIZE as i64) {
        return e;
    }
    if addrlen_ptr != 0 {
        if let Err(e) = mem::guest_slice_mut(caller, addrlen_ptr, 4) {
            return e;
        }
    }

    let peer: std::net::SocketAddr = {
        let fds = &caller.data().fds;
        match fds.get(fd) {
            Ok(Resource::Socket(s)) => match s.peer_addr {
                Some(p) => p,
                None => return -crate::errno::ENOTCONN,
            },
            Ok(_) => return -EBADF,
            Err(e) => return e,
        }
    };

    if let Ok(buf) = mem::guest_slice_mut(caller, addr_ptr, SOCKADDR_IN_SIZE as i64) {
        match peer {
            std::net::SocketAddr::V4(v4) => {
                let ip = v4.ip().octets();
                let port = v4.port();
                buf[0..2].copy_from_slice(&(AF_INET as u16).to_le_bytes());
                buf[2..4].copy_from_slice(&port.to_be_bytes());
                buf[4..8].copy_from_slice(&ip);
                for b in &mut buf[8..16] {
                    *b = 0;
                }
            }
            std::net::SocketAddr::V6(_) => return -EINVAL,
        }
    }
    if addrlen_ptr != 0 {
        if let Ok(buf) = mem::guest_slice_mut(caller, addrlen_ptr, 4) {
            buf[0..4].copy_from_slice(&(16u32).to_le_bytes());
        }
    }
    0
}

/// `shutdown(fd, how)` — half-close the connection. `how` is one of
/// `SHUT_RD` (0), `SHUT_WR` (1), `SHUT_RDWR` (2).
///
/// Sets the corresponding bits on `SocketInner.shutdown_flags`. The actual
/// effect on the underlying `TcpStream` is recorded so that subsequent
/// `recvfrom`/`sendto` calls return EOF / -EPIPE / -EIO as appropriate.
/// P1-6 doesn't actually splice the underlying stream — we just remember
/// the intent; reads on a SHUT_RD socket return 0 (EOF), writes on
/// SHUT_WR return -EPIPE.
///
/// Errors: `-EBADF`, `-EINVAL` for unknown `how`.
pub async fn shutdown(caller: &mut Caller<'_, Kernel>, a: [i64; 6]) -> i64 {
    let fd = match u32::try_from(a[0]) {
        Ok(f) => f,
        Err(_) => return -EBADF,
    };
    let how = a[1] as i32;

    let mask: u8 = match how {
        SHUT_RD => 0b01,
        SHUT_WR => 0b10,
        SHUT_RDWR => 0b11,
        _ => return -EINVAL,
    };

    let fds = &mut caller.data_mut().fds;
    match fds.get_mut(fd) {
        Ok(Resource::Socket(s)) => {
            s.shutdown_flags |= mask;
            // For an accepted/connected stream with SHUT_WR, call
            // AsyncWriteExt::shutdown on the underlying TcpStream so the
            // peer sees EOF. We take the stream out to call .await on it
            // (the borrow checker doesn't let us hold `&mut s` while
            // mutably borrowing the inner stream), then put it back.
            if (mask & 0b10) != 0 {
                let taken = s.stream.take();
                if let Some(mut stream) = taken {
                    use tokio::io::AsyncWriteExt;
                    let _ = stream.shutdown().await;
                    // Don't restore — SHUT_WR makes future writes return
                    // EPIPE; the stream is now half-closed.
                    // Dropping `stream` here closes the write half.
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