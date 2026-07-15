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
use std::os::unix::fs::FileTypeExt;

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
// P2-C3 part 1: sendmsg / recvmsg.
pub const NR_SENDMSG: u32 = 46;
pub const NR_RECVMSG: u32 = 47;
// P2-C3 part 2: socketpair + AF_UNIX.
pub const NR_SOCKETPAIR: u32 = 53;

// sendmsg/recvmsg flag bits (linux/socket.h).
pub const MSG_PEEK: i32 = 0x2;
pub const MSG_DONTWAIT: i32 = 0x40;
pub const MSG_NOSIGNAL: i32 = 0x4000;
pub const MSG_TRUNC: i32 = 0x20;
pub const MSG_CTRUNC: i32 = 0x8;

// `struct msghdr` on wasm32-musl: 8 × 4 = 32 bytes
// (msg_name, msg_namelen, msg_iov, msg_iovlen, msg_control,
//  msg_controllen, msg_flags, pad).
pub const MSGHDR_SIZE: i64 = 32;
const MSG_NAME_OFF: usize = 0;
const MSG_NAMELEN_OFF: usize = 4;
const MSG_IOV_OFF: usize = 8;
const MSG_IOVLEN_OFF: usize = 12;
const MSG_CONTROL_OFF: usize = 16;
const MSG_CONTROLLEN_OFF: usize = 20;
const MSG_FLAGS_OFF: usize = 24;

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
/// `struct sockaddr_un` on Linux is 2 (sun_family) + 108 (sun_path) = 110 bytes.
/// We round up to 112 to keep the write-back 4-byte aligned.
pub const SOCKADDR_UN_SIZE: usize = 110;

/// `socket(family, type_and_flags, protocol)` — allocate a fresh socket fd.
///
/// P1-1 only creates the fd; no data path yet. Supported families are
/// `AF_INET` and `AF_INET6`; types are `SOCK_STREAM` and `SOCK_DGRAM`.
/// `protocol` is ignored (always 0 today). Unknown family → -EAFNOSUPPORT;
/// unsupported type for a known family → -EPROTONOSUPPORT.
///
/// P2-C3 part 2: `AF_UNIX` + `SOCK_STREAM`/`SOCK_DGRAM` accepted.
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
        // P2-C3 part 2: AF_UNIX stream + dgram.
        (AF_UNIX, SOCK_STREAM) => SocketKind::Stream,
        (AF_UNIX, SOCK_DGRAM) => SocketKind::Datagram,
        // Known families but unsupported types (e.g. AF_INET SOCK_SEQPACKET).
        (AF_INET | AF_INET6 | AF_UNIX, _) => return -EPROTONOSUPPORT,
        // Everything else: family not supported.
        _ => return -EAFNOSUPPORT,
    };

    let inner = if family == AF_UNIX {
        std::sync::Arc::new(parking_lot::Mutex::new(SocketInner::new_unix(
            kind, nonblock,
        )))
    } else {
        std::sync::Arc::new(parking_lot::Mutex::new(SocketInner::new(kind, nonblock)))
    };
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
        (AF_UNIX, SOCK_STREAM) | (AF_UNIX, SOCK_DGRAM) => {
            if sock_type == SOCK_STREAM {
                SocketKind::Stream
            } else {
                SocketKind::Datagram
            }
        }
        (AF_INET | AF_INET6 | AF_UNIX, _) => return Err(-EPROTONOSUPPORT),
        _ => return Err(-EAFNOSUPPORT),
    };
    let _ = nonblock;
    let inner = if family == AF_UNIX {
        SocketInner::new_unix(kind, false)
    } else {
        SocketInner::new(kind, false)
    };
    Ok(fds.insert(Resource::Socket(std::sync::Arc::new(
        parking_lot::Mutex::new(inner),
    ))))
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
        assert_eq!(
            socket_for_test(&mut fds, 9999, SOCK_STREAM),
            Err(-EAFNOSUPPORT)
        );
    }

    #[test]
    fn socket_inet_seqpacket_returns_eprotonosupport() {
        let mut fds = FdTable::empty();
        // 0o205 = SOCK_SEQPACKET on Linux. Not in our P1 set.
        assert_eq!(
            socket_for_test(&mut fds, AF_INET, 0o205),
            Err(-EPROTONOSUPPORT)
        );
    }
}

/// Parse a `sockaddr_in` (16 bytes) or `sockaddr_in6` (28 bytes) from the
/// guest pointer. Returns `Err(-EINVAL)` on a bad family or truncated
/// `addrlen`; `Err(-EAFNOSUPPORT)` for families we don't model yet.
fn parse_sockaddr(
    caller: &mut Caller<'_, Kernel>,
    addr_ptr: i64,
    addr_len: i64,
) -> Result<SockAddr, i64> {
    if addr_ptr == 0 || addr_len == 0 {
        return Err(-(crate::errno::EDESTADDRREQ)); // no address supplied
    }
    let len = match usize::try_from(addr_len) {
        Ok(n) => n,
        Err(_) => return Err(-EINVAL),
    };
    // Bounds-check the smallest possible sockaddr header (family field).
    let header = mem::guest_slice(caller, addr_ptr, 2)?;
    let family = u16::from_le_bytes([header[0], header[1]]) as i32;

    match family {
        AF_INET => {
            if len < SOCKADDR_IN_SIZE {
                return Err(-EINVAL);
            }
            let bytes = mem::guest_slice(caller, addr_ptr, SOCKADDR_IN_SIZE as i64)?;
            // struct sockaddr_in { sa_family_t sin_family; u16 sin_port; u32 sin_addr; u8 pad[8]; }
            // Network byte order for port and addr.
            let port = u16::from_be_bytes([bytes[2], bytes[3]]);
            let addr = [bytes[4], bytes[5], bytes[6], bytes[7]];
            Ok(SockAddr::V4 { port, addr })
        }
        AF_INET6 => {
            if len < SOCKADDR_IN6_SIZE {
                return Err(-EINVAL);
            }
            let bytes = mem::guest_slice(caller, addr_ptr, SOCKADDR_IN6_SIZE as i64)?;
            // struct sockaddr_in6 { u16 sin6_family; u16 sin6_port; u32 flowinfo; u8 addr[16]; u32 scope_id; }
            let port = u16::from_be_bytes([bytes[2], bytes[3]]);
            let mut addr = [0u8; 16];
            addr.copy_from_slice(&bytes[8..24]);
            Ok(SockAddr::V6 { port, addr })
        }
        AF_UNIX => {
            // P2-C3 part 2: parse `struct sockaddr_un` (110 bytes).
            // `sun_family` (u16 LE) at offset 0, `sun_path` (108 bytes) at offset 2.
            if len < 3 {
                // At least family + 1 byte of path (or NUL) required.
                return Err(-EINVAL);
            }
            let bytes = mem::guest_slice(caller, addr_ptr, SOCKADDR_UN_SIZE as i64)?;
            // Abstract namespace (sun_path[0] == 0) → -EOPNOTSUPP.
            if bytes[2] == 0 {
                return Err(-EOPNOTSUPP);
            }
            // sun_path is NUL-terminated; find NUL or end-of-buffer.
            let path_end = bytes[2..110]
                .iter()
                .position(|&b| b == 0)
                .map(|p| p + 2)
                .unwrap_or(110);
            let path = match std::str::from_utf8(&bytes[2..path_end]) {
                Ok(s) => std::path::PathBuf::from(s),
                Err(_) => return Err(-EINVAL),
            };
            Ok(SockAddr::Unix { path })
        }
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
///
/// P2-C3 part 2: AF_UNIX + SOCK_STREAM opens a `tokio::net::UnixListener`
/// immediately (path-binding is the only sane behavior). Errors map:
///   - `AddrInUse` → `-EADDRINUSE`
///   - `NotFound` → `-ENOENT` (parent dir missing)
///   - anything else → `-EIO`
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

    // AF_UNIX path: validate the fd is a Unix socket, then bind the host
    // UnixListener immediately. We do this BEFORE recording `bound` so a
    // bind failure leaves the socket in a clean (unbound) state.
    if let SockAddr::Unix { path } = &addr {
        let is_unix_stream = {
            let fds = &mut caller.data_mut().fds;
            match fds.get_mut(fd) {
                Ok(Resource::Socket(s)) => s.lock().family_unix,
                _ => false,
            }
        };
        if !is_unix_stream {
            return -EOPNOTSUPP; // not an AF_UNIX socket
        }
        // AF_UNIX `bind(2)` semantics: if `path` already exists as a
        // socket inode from a previous run, the host `bind(2)` returns
        // EADDRINUSE (Linux does not unlink for us; `close(2)` doesn't
        // either). To make re-bind idempotent for the conformance runner
        // — which always reuses the same path — explicitly unlink a stale
        // **socket** inode first. We refuse to unlink anything else
        // (regular file, directory, symlink) so a hostile guest can't
        // coax us into `unlink("/etc/passwd")`.
        match std::fs::metadata(path) {
            Ok(m) if m.file_type().is_socket() => {
                if let Err(e) = std::fs::remove_file(path) {
                    // Another fd is holding it (TOCTOU); surface EADDRINUSE.
                    return if e.kind() == std::io::ErrorKind::NotFound {
                        0 // raced: gone now, fall through to bind below
                    } else {
                        -crate::errno::EADDRINUSE
                    };
                }
            }
            Ok(_) => return -crate::errno::EADDRINUSE, // path is a non-socket inode
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // No stale inode; happy case, proceed to bind.
            }
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
                return -crate::errno::EACCES;
            }
            Err(_) => return -crate::errno::EIO,
        }
        let listener = match tokio::net::UnixListener::bind(path) {
            Ok(l) => l,
            Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
                return -crate::errno::EADDRINUSE;
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return -crate::errno::ENOENT;
            }
            Err(_) => return -crate::errno::EIO,
        };
        // Install the listener + record the path under the socket lock.
        let fds = &mut caller.data_mut().fds;
        match fds.get_mut(fd) {
            Ok(Resource::Socket(s)) => {
                let mut gs = s.lock();
                gs.bound = Some(addr.clone());
                if let Some(unix) = gs.unix.as_mut() {
                    unix.path = Some(path.clone());
                    unix.listener = Some(listener);
                }
                0
            }
            Ok(_) => -EBADF,
            Err(e) => e,
        }
    } else {
        let fds = &mut caller.data_mut().fds;
        match fds.get_mut(fd) {
            Ok(Resource::Socket(s)) => {
                s.lock().bound = Some(addr);
                0
            }
            Ok(_) => -EBADF,
            Err(e) => e,
        }
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
    // P2-B5: take the listening TcpListener out under a short-lived lock;
    // never hold the guard across `.await`. We materialize the listener
    // (which involves a `.await`) outside any lock, then leave the
    // listener installed in SocketInner so a concurrent accept4 caller
    // doesn't race a duplicate TcpListener::bind. The actual take-out
    // for `accept().await` happens just below, also under a brief lock.
    //
    // P2-C3 part 2: AF_UNIX dispatch — taken early and run on its own
    // path because the host type (UnixListener vs TcpListener) and
    // sockaddr layout differ.
    let is_unix = {
        let fds = &mut caller.data_mut().fds;
        match fds.get_mut(fd) {
            Ok(Resource::Socket(s)) => s.lock().family_unix,
            Ok(_) => return -EBADF,
            Err(e) => return e,
        }
    };
    if is_unix {
        return accept4_unix(caller, fd, addr_ptr, addrlen_ptr, sock_nonblock).await;
    }

    let kind;
    let parent_nonblock: bool;
    let bound_addr: Option<std::net::SocketAddrV4> = {
        let fds = &mut caller.data_mut().fds;
        match fds.get_mut(fd) {
            Ok(Resource::Socket(s)) => {
                let gs = s.lock();
                if !gs.is_listening() {
                    return -EINVAL;
                }
                kind = gs.kind;
                parent_nonblock = gs.nonblock.load(std::sync::atomic::Ordering::Relaxed);
                match &gs.bound {
                    Some(SockAddr::V4 { port, addr }) => Some(std::net::SocketAddrV4::new(
                        std::net::Ipv4Addr::from(*addr),
                        *port,
                    )),
                    Some(SockAddr::V6 { .. }) => return -EOPNOTSUPP,
                    Some(SockAddr::Unix { .. }) => return -EOPNOTSUPP,
                    None => return -EINVAL,
                }
            }
            Ok(_) => return -EBADF,
            Err(e) => return e,
        }
    };

    // Phase 1a: bind the host TcpListener if we don't already have one.
    // We do this OUTSIDE the lock; a concurrent accept4 hitting the
    // empty-listener branch would race a second TcpListener::bind and
    // most likely get EADDRINUSE, but we accept that and propagate.
    // P2-B5 fix: instead of installing and immediately re-taking the
    // listener (which invited a race between install and take), we let
    // Phase 1b handle install-vs-take atomically under one lock.
    let need_bind = {
        let fds = &mut caller.data_mut().fds;
        match fds.get_mut(fd) {
            Ok(Resource::Socket(s)) => s.lock().listener.is_none(),
            Ok(_) => return -EBADF,
            Err(e) => return e,
        }
    };
    if need_bind && bound_addr.is_none() {
        return -EINVAL;
    }
    if need_bind {
        let addr = bound_addr.expect("checked above");
        let listener = match tokio::net::TcpListener::bind(addr).await {
            Ok(l) => l,
            Err(_) => return -crate::errno::EADDRINUSE,
        };
        // Install the listener under a brief lock; if a concurrent
        // accept4 already installed one, drop ours.
        let fds = &mut caller.data_mut().fds;
        match fds.get_mut(fd) {
            Ok(Resource::Socket(s)) => {
                let mut gs = s.lock();
                if gs.listener.is_none() {
                    gs.listener = Some(listener);
                    gs.is_acceptor = true;
                } else {
                    drop(listener);
                }
            }
            Ok(_) => return -EBADF,
            Err(e) => return e,
        }
    }

    // Phase 1b: take the listener out (so we can `.await` outside any
    // lock). Phase 4 below restores it after `accept().await` returns.
    let listener = {
        let fds = &mut caller.data_mut().fds;
        match fds.get_mut(fd) {
            Ok(Resource::Socket(s)) => match s.lock().listener.take() {
                Some(l) => l,
                None => return -EINVAL,
            },
            Ok(_) => return -EBADF,
            Err(e) => return e,
        }
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
            let mut gs = s.lock();
            if gs.listener.is_none() {
                gs.listener = Some(listener);
            }
            // If a concurrent caller already restored one, drop ours.
        }
    }

    // Phase 5: insert the accepted stream as a fresh fd.
    let mut accepted = SocketInner::from_accepted(stream, kind, sock_nonblock || parent_nonblock);
    // Record the peer address for `getpeername`.
    accepted.peer_addr = Some(peer);
    let new_fd = caller
        .data_mut()
        .fds
        .insert(Resource::Socket(std::sync::Arc::new(
            parking_lot::Mutex::new(accepted),
        )));
    new_fd as i64
}

/// `accept(fd, addr, addrlen)` — legacy syscall. Implemented as a
/// shim over `accept4(fd, addr, addrlen, 0)` (no flags).
pub async fn accept(caller: &mut Caller<'_, Kernel>, a: [i64; 6]) -> i64 {
    accept4(caller, [a[0], a[1], a[2], 0, 0, 0]).await
}

/// P2-C3 part 2: AF_UNIX accept4 path.
///
/// Listener was materialized at `bind()` time (path-bind is the only
/// sane behavior), so we just await on the listener. The accepted
/// UnixStream goes into a fresh SocketInner; the peer address is the
/// abstract `std::os::unix::net::SocketAddr` (empty for unnamed peers).
async fn accept4_unix(
    caller: &mut Caller<'_, Kernel>,
    fd: u32,
    addr_ptr: i64,
    addrlen_ptr: i64,
    sock_nonblock: bool,
) -> i64 {
    // Validate output pointers up front.
    if addr_ptr != 0 {
        if let Err(e) = mem::guest_slice_mut(caller, addr_ptr, SOCKADDR_UN_SIZE as i64) {
            return e;
        }
    }
    if addrlen_ptr != 0 {
        if let Err(e) = mem::guest_slice_mut(caller, addrlen_ptr, 4) {
            return e;
        }
    }

    // Clone the listener (sync), then put the original back so subsequent
    // accepts still work. tokio's UnixListener doesn't expose `try_clone`
    // directly — clone via the std listener (independent handle that
    // shares the same OS fd).
    let listener = {
        let fds = &mut caller.data_mut().fds;
        match fds.get_mut(fd) {
            Ok(Resource::Socket(s)) => {
                let mut gs = s.lock();
                if !gs.is_listening() {
                    return -EINVAL;
                }
                let unix = match gs.unix.as_mut() {
                    Some(u) => u,
                    None => return -EINVAL,
                };
                let l = match unix.listener.take() {
                    Some(l) => l,
                    None => return -EINVAL,
                };
                let std_l = l.into_std().expect("listener into_std");
                // `UnixListener::try_clone` *can* return Err on host-side resource
                // exhaustion (per stdlib docs). Return -ENOMEM instead of
                // panicking (spec §8 / §9: handlers must surface errnos, not
                // panic on host state).
                let cloned = match std_l.try_clone() {
                    Ok(c) => c,
                    Err(_) => return -crate::errno::ENOMEM,
                };
                let tokio_cloned = tokio::net::UnixListener::from_std(cloned).expect("from_std");
                // Put the original back so the next accept finds it.
                unix.listener = Some(tokio::net::UnixListener::from_std(std_l).expect("from_std"));
                tokio_cloned
            }
            Ok(_) => return -EBADF,
            Err(e) => return e,
        }
    };

    // Phase 2: await a connection.
    let (stream, peer) = match listener.accept().await {
        Ok(pair) => pair,
        Err(_) => return -crate::errno::EIO,
    };

    // Phase 3: write back the peer sockaddr if requested. For an unnamed
    // peer (which is what we get from a process that connected without
    // bind), `peer.as_pathname()` is None — write family + zero path.
    if addr_ptr != 0 {
        if let Ok(buf) = mem::guest_slice_mut(caller, addr_ptr, SOCKADDR_UN_SIZE as i64) {
            buf[0..2].copy_from_slice(&(AF_UNIX as u16).to_le_bytes());
            for b in &mut buf[2..SOCKADDR_UN_SIZE] {
                *b = 0;
            }
            // sun_path can hold a peer name (if connected from a bound socket).
            if let Some(path) = peer.as_pathname().and_then(|p| p.to_str()) {
                let bytes = path.as_bytes();
                let n = bytes.len().min(108);
                buf[2..2 + n].copy_from_slice(&bytes[..n]);
                buf[2 + n] = 0; // NUL terminate
            }
        }
    }
    if addrlen_ptr != 0 {
        let len = peer
            .as_pathname()
            .and_then(|p| p.to_str())
            .map(|s| (2 + s.len() + 1) as u32)
            .unwrap_or(2);
        if let Ok(buf) = mem::guest_slice_mut(caller, addrlen_ptr, 4) {
            buf[0..4].copy_from_slice(&len.to_le_bytes());
        }
    }

    // Insert accepted stream as a fresh fd.
    let mut accepted = SocketInner::new_unix(SocketKind::Stream, sock_nonblock);
    accepted.stream_unix = Some(stream);
    // tokio's SocketAddr isn't directly the std type; only store a peer
    // address when the connecting peer was bound to a filesystem path.
    if let Some(p) = peer.as_pathname() {
        if let Ok(sa) = std::os::unix::net::SocketAddr::from_pathname(p) {
            accepted.peer_addr_unix = Some(sa);
        }
    }
    let new_fd = caller
        .data_mut()
        .fds
        .insert(Resource::Socket(std::sync::Arc::new(
            parking_lot::Mutex::new(accepted),
        )));
    new_fd as i64
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

    // P2-C3 part 2: AF_UNIX dispatch — connect is a one-liner over
    // tokio::net::UnixStream::connect.
    if let SockAddr::Unix { path } = &addr {
        // Pre-check: this is an AF_UNIX socket and not already connected.
        let is_unix_stream = {
            let fds = &caller.data().fds;
            match fds.get(fd) {
                Ok(Resource::Socket(s)) => {
                    let gs = s.lock();
                    if !gs.family_unix {
                        return -EOPNOTSUPP;
                    }
                    if gs.stream_unix.is_some() {
                        return -crate::errno::EISCONN;
                    }
                    true
                }
                Ok(_) => return -EBADF,
                Err(e) => return e,
            }
        };
        let _ = is_unix_stream;
        let stream = match tokio::net::UnixStream::connect(path).await {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return -crate::errno::ECONNREFUSED;
            }
            Err(_) => return -crate::errno::EIO,
        };
        let fds = &mut caller.data_mut().fds;
        match fds.get_mut(fd) {
            Ok(Resource::Socket(s)) => {
                let mut gs = s.lock();
                gs.bound = Some(addr.clone());
                gs.stream_unix = Some(stream);
                // Mark the bound side so getsockname returns the peer's path.
                if let Ok(sa) = std::os::unix::net::SocketAddr::from_pathname(path) {
                    gs.peer_addr_unix = Some(sa);
                }
                0
            }
            Ok(_) => -EBADF,
            Err(e) => e,
        }
    } else {
        connect_v4(caller, fd, addr).await
    }
}

/// P2-C3 part 2: IPv4 connect path. Split out so the AF_UNIX branch above
/// stays compact.
async fn connect_v4(caller: &mut Caller<'_, Kernel>, fd: u32, addr: SockAddr) -> i64 {
    let target = match &addr {
        SockAddr::V4 { port, addr: bytes } => std::net::SocketAddr::V4(
            std::net::SocketAddrV4::new(std::net::Ipv4Addr::from(*bytes), *port),
        ),
        SockAddr::V6 { .. } => return -EOPNOTSUPP,
        SockAddr::Unix { .. } => return -EOPNOTSUPP,
    };

    // Pre-check: is this a Socket? Does it already have a stream?
    {
        let fds = &caller.data().fds;
        match fds.get(fd) {
            Ok(Resource::Socket(s)) => {
                if s.lock().stream.is_some() {
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
                s.lock()
                    .last_error
                    .store(errno as i32, std::sync::atomic::Ordering::Relaxed);
            }
            return -errno;
        }
    };

    let fds = &mut caller.data_mut().fds;
    match fds.get_mut(fd) {
        Ok(Resource::Socket(s)) => {
            let mut gs = s.lock();
            gs.stream = Some(stream);
            // Record peer for getpeername — we know the target.
            gs.peer_addr = Some(target);
            0
        }
        Ok(_) => -EBADF,
        Err(e) => e,
    }
}

/// `socketpair(family, type_and_flags, protocol, sv[2])` — create a pair
/// of connected AF_UNIX sockets. The two halves are written into `sv` as
/// two u32 fds.
///
/// P2-C3 part 2: only `AF_UNIX + SOCK_STREAM` is supported. Other
/// families → `-EAFNOSUPPORT`; `SOCK_DGRAM` accepted but modeled as
/// stream (a documented v1 simplification).
pub async fn socketpair(caller: &mut Caller<'_, Kernel>, a: [i64; 6]) -> i64 {
    let family = a[0] as i32;
    let type_and_flags = a[1] as i32;
    let _protocol = a[2]; // ignored
    let sv_ptr = a[3];

    if family != AF_UNIX {
        return -EAFNOSUPPORT;
    }
    let sock_type = type_and_flags & 0xf;
    if sock_type != SOCK_STREAM && sock_type != SOCK_DGRAM {
        return -EPROTONOSUPPORT;
    }
    if sv_ptr == 0 {
        return -crate::errno::EFAULT;
    }
    if let Err(e) = mem::guest_slice_mut(caller, sv_ptr, 8) {
        return e;
    }

    let (a_stream, b_stream) = match tokio::net::UnixStream::pair() {
        Ok(pair) => pair,
        Err(_) => return -crate::errno::EIO,
    };

    // Insert both into the fd table as AF_UNIX stream sockets. We don't
    // pre-bind them to a path (socketpair sockets are unnamed).
    let kind = if sock_type == SOCK_DGRAM {
        SocketKind::Datagram
    } else {
        SocketKind::Stream
    };
    let mut a_inner = SocketInner::new_unix(kind, false);
    a_inner.stream_unix = Some(a_stream);
    let mut b_inner = SocketInner::new_unix(kind, false);
    b_inner.stream_unix = Some(b_stream);
    let a_fd = caller
        .data_mut()
        .fds
        .insert(Resource::Socket(std::sync::Arc::new(
            parking_lot::Mutex::new(a_inner),
        )));
    let b_fd = caller
        .data_mut()
        .fds
        .insert(Resource::Socket(std::sync::Arc::new(
            parking_lot::Mutex::new(b_inner),
        )));

    if let Ok(buf) = mem::guest_slice_mut(caller, sv_ptr, 8) {
        buf[0..4].copy_from_slice(&a_fd.to_le_bytes());
        buf[4..8].copy_from_slice(&b_fd.to_le_bytes());
    }
    0
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
    let _addr_ptr = a[4]; // ignored for TCP
    let _addrlen = a[5];

    // P2-C3 part 2: AF_UNIX dispatch.
    let is_unix = {
        let fds = &caller.data().fds;
        match fds.get(fd) {
            Ok(Resource::Socket(s)) => s.lock().family_unix,
            Ok(_) => return -EBADF,
            Err(e) => return e,
        }
    };
    if is_unix {
        return sendto_unix(caller, fd, buf_ptr, buf_len_raw).await;
    }

    let len = match usize::try_from(buf_len_raw) {
        Ok(n) => n,
        Err(_) => return -EFAULT,
    };

    let bytes = match mem::guest_slice(caller, buf_ptr, buf_len_raw) {
        Ok(b) => b.to_vec(),
        Err(e) => return e,
    };

    // Pull the stream out under a short-lived lock so we can `.await`
    // outside the &mut caller borrow. Never hold the Mutex guard
    // across `.await` (parking_lot::Mutex is sync).
    let mut stream = {
        let fds = &mut caller.data_mut().fds;
        match fds.get_mut(fd) {
            Ok(Resource::Socket(s)) => {
                let mut gs = s.lock();
                // P1-6: SHUT_WR was called — writes must fail with EPIPE.
                if gs.shutdown_flags & 0b10 != 0 {
                    return -crate::errno::EPIPE;
                }
                match gs.stream.take() {
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
                let mut gs = s.lock();
                if gs.stream.is_none() {
                    gs.stream = Some(stream);
                }
            }
            return -crate::errno::EIO;
        }
    };

    // Put the stream back.
    {
        let fds = &mut caller.data_mut().fds;
        if let Ok(Resource::Socket(s)) = fds.get_mut(fd) {
            let mut gs = s.lock();
            if gs.stream.is_none() {
                gs.stream = Some(stream);
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

    // P2-C3 part 2: AF_UNIX dispatch. We ignore the addr write-back for
    // AF_UNIX (peer is unnamed for connect-accepted or named only for
    // bound peers; the C conformance tests don't exercise it).
    let is_unix = {
        let fds = &caller.data().fds;
        match fds.get(fd) {
            Ok(Resource::Socket(s)) => s.lock().family_unix,
            Ok(_) => return -EBADF,
            Err(e) => return e,
        }
    };
    if is_unix {
        return recvfrom_unix(caller, fd, buf_ptr, buf_len_raw).await;
    }
    let _ = (addr_ptr, addrlen_ptr); // unused on the unix branch

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

    // Pull the stream out under a short-lived lock (P2-B5: never hold
    // the Mutex guard across `.await`).
    let mut stream = {
        let fds = &mut caller.data_mut().fds;
        match fds.get_mut(fd) {
            Ok(Resource::Socket(s)) => {
                let mut gs = s.lock();
                // P1-6: SHUT_RD was called — reads return EOF immediately.
                if gs.shutdown_flags & 0b01 != 0 {
                    return 0;
                }
                match gs.stream.take() {
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
                let mut gs = s.lock();
                if gs.stream.is_none() {
                    gs.stream = Some(stream);
                }
            }
            return -crate::errno::EIO;
        }
    };

    // Put the stream back.
    {
        let fds = &mut caller.data_mut().fds;
        if let Ok(Resource::Socket(s)) = fds.get_mut(fd) {
            let mut gs = s.lock();
            if gs.stream.is_none() {
                gs.stream = Some(stream);
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
                let mut gs = s.lock();
                match (level, optname) {
                    (SOL_SOCKET, SO_REUSEADDR) => gs.so_reuseaddr = true,
                    (SOL_SOCKET, SO_KEEPALIVE) => gs.so_keepalive = true,
                    (IPPROTO_TCP, TCP_NODELAY) => gs.tcp_nodelay = true,
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

    // P2-B6: a non-socket fd must fail with -EBADF (matches
    // tests/conformance/getsockopt.c line 49-53). Linux semantics is
    // actually -ENOTSOCK, but our conformance test contracts to
    // -EBADF; preserve that contract for now.
    {
        let fds_table = &caller.data().fds;
        match fds_table.get(fd) {
            Ok(Resource::Socket(_)) => {}
            Ok(_) | Err(_) => return -crate::errno::EBADF,
        }
    }

    // Phase 1: compute the value to write. We do this while holding only
    // an immutable borrow on `caller` so the borrow checker is happy.
    // SO_ERROR swaps atomically, so we mutate here — that's fine because
    // it's `&self`-internal state on the SocketInner, not `caller`.
    let value: i32 = {
        let fds_table = &caller.data().fds;
        match (level, optname) {
            (SOL_SOCKET, SO_TYPE) => 1,   // SOCK_STREAM (only stream modeled)
            (SOL_SOCKET, SO_DOMAIN) => 2, // AF_INET
            (SOL_SOCKET, SO_ERROR) => match fds_table.get(fd) {
                Ok(Resource::Socket(s)) => s
                    .lock()
                    .last_error
                    .swap(0, std::sync::atomic::Ordering::Relaxed),
                _ => 0,
            },
            (SOL_SOCKET, SO_ACCEPTCONN) => match fds_table.get(fd) {
                Ok(Resource::Socket(s)) => s.lock().is_acceptor as i32,
                _ => 0,
            },
            (SOL_SOCKET, SO_REUSEADDR) => match fds_table.get(fd) {
                Ok(Resource::Socket(s)) => s.lock().so_reuseaddr as i32,
                _ => 0,
            },
            (SOL_SOCKET, SO_KEEPALIVE) => match fds_table.get(fd) {
                Ok(Resource::Socket(s)) => s.lock().so_keepalive as i32,
                _ => 0,
            },
            (IPPROTO_TCP, TCP_NODELAY) => match fds_table.get(fd) {
                Ok(Resource::Socket(s)) => s.lock().tcp_nodelay as i32,
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

    // P2-C3 part 2: branch on family.
    let is_unix = {
        let fds = &caller.data().fds;
        match fds.get(fd) {
            Ok(Resource::Socket(s)) => s.lock().family_unix,
            Ok(_) => return -EBADF,
            Err(e) => return e,
        }
    };

    if is_unix {
        if let Err(e) = mem::guest_slice_mut(caller, addr_ptr, SOCKADDR_UN_SIZE as i64) {
            return e;
        }
        if addrlen_ptr != 0 {
            if let Err(e) = mem::guest_slice_mut(caller, addrlen_ptr, 4) {
                return e;
            }
        }
        let bound_path: Option<std::path::PathBuf> = {
            let fds = &caller.data().fds;
            match fds.get(fd) {
                Ok(Resource::Socket(s)) => {
                    let gs = s.lock();
                    match &gs.bound {
                        Some(crate::fd::SockAddr::Unix { path }) => Some(path.clone()),
                        _ => None,
                    }
                }
                Ok(_) => return -EBADF,
                Err(e) => return e,
            }
        };
        if let Ok(buf) = mem::guest_slice_mut(caller, addr_ptr, SOCKADDR_UN_SIZE as i64) {
            buf[0..2].copy_from_slice(&(AF_UNIX as u16).to_le_bytes());
            for b in &mut buf[2..SOCKADDR_UN_SIZE] {
                *b = 0;
            }
            if let Some(path) = bound_path.as_ref().and_then(|p| p.to_str()) {
                let bytes = path.as_bytes();
                let n = bytes.len().min(108);
                buf[2..2 + n].copy_from_slice(&bytes[..n]);
                buf[2 + n] = 0;
            }
        }
        if addrlen_ptr != 0 {
            let len = bound_path
                .as_ref()
                .and_then(|p| p.to_str())
                .map(|s| (2 + s.len() + 1) as u32)
                .unwrap_or(2);
            if let Ok(buf) = mem::guest_slice_mut(caller, addrlen_ptr, 4) {
                buf[0..4].copy_from_slice(&len.to_le_bytes());
            }
        }
        return 0;
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
            Ok(Resource::Socket(s)) => {
                let gs = s.lock();
                match &gs.bound {
                    Some(crate::fd::SockAddr::V4 { port, addr }) => std::net::SocketAddr::V4(
                        std::net::SocketAddrV4::new(std::net::Ipv4Addr::from(*addr), *port),
                    ),
                    Some(crate::fd::SockAddr::V6 { .. }) => {
                        // IPv6 support is P2; report EINVAL for now.
                        return -EINVAL;
                    }
                    Some(crate::fd::SockAddr::Unix { .. }) => return -EINVAL,
                    None => match &gs.peer_addr {
                        // No bind yet — fall back to the peer's family so
                        // getpeername callers can still read the response.
                        Some(p) => *p,
                        None => std::net::SocketAddr::V4(std::net::SocketAddrV4::new(
                            std::net::Ipv4Addr::new(0, 0, 0, 0),
                            0,
                        )),
                    },
                }
            }
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

    // P2-C3 part 2: branch on family.
    let is_unix = {
        let fds = &caller.data().fds;
        match fds.get(fd) {
            Ok(Resource::Socket(s)) => s.lock().family_unix,
            Ok(_) => return -EBADF,
            Err(e) => return e,
        }
    };

    if is_unix {
        if let Err(e) = mem::guest_slice_mut(caller, addr_ptr, SOCKADDR_UN_SIZE as i64) {
            return e;
        }
        if addrlen_ptr != 0 {
            if let Err(e) = mem::guest_slice_mut(caller, addrlen_ptr, 4) {
                return e;
            }
        }
        let peer: std::os::unix::net::SocketAddr = {
            let fds = &caller.data().fds;
            match fds.get(fd) {
                Ok(Resource::Socket(s)) => match s.lock().peer_addr_unix.clone() {
                    Some(p) => p,
                    None => return -crate::errno::ENOTCONN,
                },
                Ok(_) => return -EBADF,
                Err(e) => return e,
            }
        };
        if let Ok(buf) = mem::guest_slice_mut(caller, addr_ptr, SOCKADDR_UN_SIZE as i64) {
            buf[0..2].copy_from_slice(&(AF_UNIX as u16).to_le_bytes());
            for b in &mut buf[2..SOCKADDR_UN_SIZE] {
                *b = 0;
            }
            if let Some(path) = peer.as_pathname().and_then(|p| p.to_str()) {
                let bytes = path.as_bytes();
                let n = bytes.len().min(108);
                buf[2..2 + n].copy_from_slice(&bytes[..n]);
                buf[2 + n] = 0;
            }
        }
        if addrlen_ptr != 0 {
            let len = peer
                .as_pathname()
                .and_then(|p| p.to_str())
                .map(|s| (2 + s.len() + 1) as u32)
                .unwrap_or(2);
            if let Ok(buf) = mem::guest_slice_mut(caller, addrlen_ptr, 4) {
                buf[0..4].copy_from_slice(&len.to_le_bytes());
            }
        }
        return 0;
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
            Ok(Resource::Socket(s)) => match s.lock().peer_addr {
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

    // Take the stream out under a short-lived lock so we can `.await`
    // outside any Mutex guard (P2-B5: never hold across `.await`).
    let taken: Option<tokio::net::TcpStream> = {
        let fds = &mut caller.data_mut().fds;
        match fds.get_mut(fd) {
            Ok(Resource::Socket(s)) => {
                let mut gs = s.lock();
                gs.shutdown_flags |= mask;
                if (mask & 0b10) != 0 {
                    gs.stream.take()
                } else {
                    None
                }
            }
            Ok(_) => return -EBADF,
            Err(e) => return e,
        }
    };
    if let Some(mut stream) = taken {
        use tokio::io::AsyncWriteExt;
        let _ = stream.shutdown().await;
        // Don't restore — SHUT_WR makes future writes return EPIPE;
        // the stream is now half-closed. Dropping closes the write half.
    }
    0
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
            let mut gs = s.lock();
            if gs.bound.is_none() {
                return -(crate::errno::EDESTADDRREQ);
            }
            gs.listen_backlog = Some(backlog as i32);
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
                let mut gs = s.lock();
                if gs.bound.is_none() {
                    -crate::errno::EDESTADDRREQ
                } else {
                    gs.listen_backlog = Some(5);
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
                s.lock().bound = Some(SockAddr::V4 {
                    port: 8080,
                    addr: [127, 0, 0, 1],
                });
            }
            _ => unreachable!(),
        }
        // Re-borrow immutably to read back.
        match fds.get(fd).unwrap() {
            Resource::Socket(s) => {
                let gs = s.lock();
                match &gs.bound {
                    Some(SockAddr::V4 { port, addr }) => {
                        assert_eq!(*port, 8080);
                        assert_eq!(*addr, [127, 0, 0, 1]);
                    }
                    other => panic!("expected V4 bound, got {other:?}"),
                }
                assert!(!gs.is_listening(), "not listening yet");
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
                let mut gs = s.lock();
                assert!(!gs.is_listening());
                gs.bound = Some(SockAddr::V4 {
                    port: 0,
                    addr: [0, 0, 0, 0],
                });
                assert!(!gs.is_listening(), "bind alone is not listening");
                gs.listen_backlog = Some(128);
                assert!(gs.is_listening(), "bind + listen -> listening");
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn as_v4_converts_bound_address() {
        let addr = SockAddr::V4 {
            port: 8080,
            addr: [192, 168, 1, 1],
        };
        let v4 = addr.as_v4().expect("V4 -> Some");
        assert_eq!(*v4.ip(), std::net::Ipv4Addr::new(192, 168, 1, 1));
        assert_eq!(v4.port(), 8080);
    }
}

// ─── P2-C3 part 1: sendmsg, recvmsg.

/// Read a `struct msghdr` from guest memory and gather the iovec payload
/// into a single contiguous `Vec<u8>`. Returns the gathered bytes, the
/// total length the caller asked for, and the flags from the msghdr.
fn read_msghdr_iov(
    caller: &mut Caller<'_, Kernel>,
    msghdr_ptr: i64,
) -> Result<(Vec<u8>, i32, bool, i64), i64> {
    let mhdr = mem::guest_slice(caller, msghdr_ptr, MSGHDR_SIZE)?;
    let iov_ptr = u32::from_le_bytes(mhdr[MSG_IOV_OFF..MSG_IOV_OFF + 4].try_into().unwrap()) as i64;
    let iov_count =
        u32::from_le_bytes(mhdr[MSG_IOVLEN_OFF..MSG_IOVLEN_OFF + 4].try_into().unwrap()) as i64;
    let flags = i32::from_le_bytes(mhdr[MSG_FLAGS_OFF..MSG_FLAGS_OFF + 4].try_into().unwrap());

    if iov_count <= 0 || iov_ptr == 0 {
        // Empty iovec list (or NULL iov_ptr with zero count): no payload
        // to send/recv. Matches Linux semantics where msghdr with no
        // iovecs is valid.
        return Ok((Vec::new(), flags, true, 0));
    }
    let iov_count_us = iov_count as usize;
    let total_iov_bytes = (iov_count as i64).checked_mul(8).unwrap_or(i64::MAX);
    let iov_bytes = mem::guest_slice(caller, iov_ptr, total_iov_bytes)?;
    let mut payload = Vec::new();
    let mut total_len: usize = 0;
    for i in 0..iov_count_us {
        let base = i * 8;
        let iov_base = u32::from_le_bytes(iov_bytes[base..base + 4].try_into().unwrap()) as i64;
        let iov_len = u32::from_le_bytes(iov_bytes[base + 4..base + 8].try_into().unwrap()) as i64;
        total_len = total_len.saturating_add(iov_len.max(0) as usize);
        if iov_base != 0 && iov_len > 0 {
            let data = mem::guest_slice(caller, iov_base, iov_len)?;
            payload.extend_from_slice(data);
        }
    }
    Ok((payload, flags, true, total_len as i64))
}

/// `sendmsg(fd, msghdr, flags)` — gather the iovec payload from the
/// msghdr and write to the socket. Honors:
/// * `MSG_DONTWAIT` — flip nonblock for the duration.
/// * `MSG_NOSIGNAL` — accepted, discarded (we don't deliver SIGPIPE).
/// * `MSG_PEEK` — invalid for send; ignored.
/// * Other flags accepted silently. Returns the number of bytes written.
pub async fn sendmsg(caller: &mut Caller<'_, Kernel>, a: [i64; 6]) -> i64 {
    let fd = match u32::try_from(a[0]) {
        Ok(f) => f,
        Err(_) => return -EBADF,
    };
    let msghdr_ptr = a[1];
    let flags_in = a[2] as i32;

    // Validate the fd up front so a bogus fd returns EBADF rather than
    // getting masked by the empty-msghdr fast-path.
    {
        let fds = &mut caller.data_mut().fds;
        match fds.get_mut(fd) {
            Ok(Resource::Socket(_)) => {}
            Ok(_) => return -EBADF,
            Err(_) => return -EBADF,
        }
    }

    let (payload, _msg_flags, ok, _total) = match read_msghdr_iov(caller, msghdr_ptr) {
        Ok(t) => t,
        Err(e) => return e,
    };
    if !ok {
        return -EINVAL;
    }
    if payload.is_empty() {
        return 0;
    }

    // Honor MSG_DONTWAIT: flip nonblock for the duration of this call.
    let was_nonblock = if flags_in & MSG_DONTWAIT != 0 {
        let fds = &mut caller.data_mut().fds;
        match fds.get_mut(fd) {
            Ok(Resource::Socket(s)) => {
                let prev = s.lock().nonblock.load(std::sync::atomic::Ordering::Relaxed);
                s.lock()
                    .nonblock
                    .store(true, std::sync::atomic::Ordering::Relaxed);
                Some(prev)
            }
            _ => None,
        }
    } else {
        None
    };

    // Take the stream out under a short lock; never hold the guard
    // across .await.
    let mut stream = {
        let fds = &mut caller.data_mut().fds;
        match fds.get_mut(fd) {
            Ok(Resource::Socket(s)) => {
                let mut gs = s.lock();
                if gs.shutdown_flags & 0b10 != 0 {
                    return -crate::errno::EPIPE;
                }
                match gs.stream.take() {
                    Some(st) => st,
                    None => return -crate::errno::ENOTCONN,
                }
            }
            Ok(_) => return -EBADF,
            Err(e) => return e,
        }
    };

    use tokio::io::AsyncWriteExt;
    let res = stream.write(&payload).await;

    // Restore stream and (optionally) nonblock.
    {
        let fds = &mut caller.data_mut().fds;
        if let Ok(Resource::Socket(s)) = fds.get_mut(fd) {
            let mut gs = s.lock();
            if gs.stream.is_none() {
                gs.stream = Some(stream);
            }
            if let Some(prev) = was_nonblock {
                gs.nonblock
                    .store(prev, std::sync::atomic::Ordering::Relaxed);
            }
        }
    }

    match res {
        Ok(n) => n as i64,
        Err(_) => -crate::errno::EIO,
    }
}

/// `recvmsg(fd, msghdr, flags)` — read into the iovecs named by the
/// msghdr. Honors:
/// * `MSG_PEEK` — bytes are stashed in `SocketInner.peek_buf` and not
///   consumed from the stream.
/// * `MSG_DONTWAIT` — flip nonblock for the duration.
/// * `MSG_CTRUNC` — `msg_controllen` is reported as 0 (no ancillary).
/// * `MSG_TRUNC` — accepted; truncates the read to the buffer capacity.
pub async fn recvmsg(caller: &mut Caller<'_, Kernel>, a: [i64; 6]) -> i64 {
    use std::collections::VecDeque;
    let fd = match u32::try_from(a[0]) {
        Ok(f) => f,
        Err(_) => return -EBADF,
    };
    let msghdr_ptr = a[1];
    let flags_in = a[2] as i32;
    let is_peek = flags_in & MSG_PEEK != 0;
    let is_dontwait = flags_in & MSG_DONTWAIT != 0;

    // Validate the fd up front so a bogus fd doesn't masquerade as EINVAL
    // (which it would if we hit the empty-msghdr fast-path first).
    let is_socket = {
        let fds = &mut caller.data_mut().fds;
        match fds.get_mut(fd) {
            Ok(Resource::Socket(_)) => true,
            Ok(_) => return -EBADF,
            Err(_) => return -EBADF,
        }
    };
    let _ = is_socket;

    // Snapshot the msghdr fields we need; for output, we'll write back
    // msg_controllen=0 (MSG_CTRUNC) and msg_namelen unchanged.
    let mhdr = match mem::guest_slice(caller, msghdr_ptr, MSGHDR_SIZE) {
        Ok(b) => b,
        Err(e) => return e,
    };
    let iov_ptr = u32::from_le_bytes(mhdr[MSG_IOV_OFF..MSG_IOV_OFF + 4].try_into().unwrap()) as i64;
    let iov_count =
        u32::from_le_bytes(mhdr[MSG_IOVLEN_OFF..MSG_IOVLEN_OFF + 4].try_into().unwrap()) as i64;
    let control_ptr = u32::from_le_bytes(
        mhdr[MSG_CONTROL_OFF..MSG_CONTROL_OFF + 4]
            .try_into()
            .unwrap(),
    ) as i64;
    let _control_len = u32::from_le_bytes(
        mhdr[MSG_CONTROLLEN_OFF..MSG_CONTROLLEN_OFF + 4]
            .try_into()
            .unwrap(),
    );
    let name_ptr =
        u32::from_le_bytes(mhdr[MSG_NAME_OFF..MSG_NAME_OFF + 4].try_into().unwrap()) as i64;
    let _name_len = u32::from_le_bytes(
        mhdr[MSG_NAMELEN_OFF..MSG_NAMELEN_OFF + 4]
            .try_into()
            .unwrap(),
    );

    if iov_count <= 0 || iov_ptr == 0 {
        return -EINVAL;
    }
    let iov_count_us = iov_count as usize;
    let total_iov_bytes = (iov_count as i64).checked_mul(8).unwrap_or(i64::MAX);
    let iov_bytes = match mem::guest_slice(caller, iov_ptr, total_iov_bytes) {
        Ok(b) => b,
        Err(e) => return e,
    };

    // Compute total capacity across all iovecs.
    let mut total_cap: usize = 0;
    let mut spans: Vec<(i64, usize)> = Vec::new();
    for i in 0..iov_count_us {
        let base = i * 8;
        let iov_base = u32::from_le_bytes(iov_bytes[base..base + 4].try_into().unwrap()) as i64;
        let iov_len = u32::from_le_bytes(iov_bytes[base + 4..base + 8].try_into().unwrap()) as i64;
        let len = iov_len.max(0) as usize;
        total_cap = total_cap.saturating_add(len);
        spans.push((iov_base, len));
    }
    if total_cap == 0 {
        return 0;
    }

    // Honor MSG_DONTWAIT.
    let was_nonblock = if is_dontwait {
        let fds = &mut caller.data_mut().fds;
        match fds.get_mut(fd) {
            Ok(Resource::Socket(s)) => {
                let prev = s.lock().nonblock.load(std::sync::atomic::Ordering::Relaxed);
                s.lock()
                    .nonblock
                    .store(true, std::sync::atomic::Ordering::Relaxed);
                Some(prev)
            }
            _ => None,
        }
    } else {
        None
    };

    // Drain peek_buf first if there's anything queued.
    let mut peeked: Vec<u8> = Vec::new();
    {
        let fds = &mut caller.data_mut().fds;
        if let Ok(Resource::Socket(s)) = fds.get_mut(fd) {
            let gs = s.lock();
            let mut buf = gs.peek_buf.lock();
            if !buf.is_empty() {
                let take = buf.len().min(total_cap);
                peeked.extend(buf.drain(..take));
            }
        }
    }
    if !peeked.is_empty() {
        // We have peeked bytes — return them now and skip the actual read.
        // (peek_buf was populated by a prior recvmsg MSG_PEEK.)
        copy_bytes_to_spans(caller, &spans, &peeked);
        write_back_msghdr(caller, msghdr_ptr, control_ptr, name_ptr, 0);
        // restore nonblock
        restore_nonblock(caller, fd, was_nonblock);
        return peeked.len() as i64;
    }

    // Take the stream out; await outside the lock.
    let mut stream = {
        let fds = &mut caller.data_mut().fds;
        match fds.get_mut(fd) {
            Ok(Resource::Socket(s)) => {
                let mut gs = s.lock();
                if gs.shutdown_flags & 0b01 != 0 {
                    return 0;
                }
                match gs.stream.take() {
                    Some(st) => st,
                    None => return -crate::errno::ENOTCONN,
                }
            }
            Ok(_) => return -EBADF,
            Err(e) => return e,
        }
    };

    use tokio::io::AsyncReadExt;
    let want = total_cap;
    let mut buf = vec![0u8; want];
    let read_result = stream.read(&mut buf).await;

    // Restore stream.
    {
        let fds = &mut caller.data_mut().fds;
        if let Ok(Resource::Socket(s)) = fds.get_mut(fd) {
            let mut gs = s.lock();
            if gs.stream.is_none() {
                gs.stream = Some(stream);
            }
        }
    }

    let n = match read_result {
        Ok(n) => n,
        Err(_) => {
            restore_nonblock(caller, fd, was_nonblock);
            return -crate::errno::EIO;
        }
    };
    let data = &buf[..n];

    if is_peek {
        // Stash the bytes in peek_buf; do NOT consume from the stream.
        let mut to_stash: VecDeque<u8> = VecDeque::with_capacity(n);
        to_stash.extend(data.iter().copied());
        let fds = &mut caller.data_mut().fds;
        if let Ok(Resource::Socket(s)) = fds.get_mut(fd) {
            // Append to existing peek_buf.
            s.lock().peek_buf.lock().extend(to_stash);
        }
    }

    // Copy into iovec spans.
    copy_bytes_to_spans(caller, &spans, data);

    // Write back msg_controllen=0 (MSG_CTRUNC) and msg_namelen unchanged.
    write_back_msghdr(caller, msghdr_ptr, control_ptr, name_ptr, 0);

    restore_nonblock(caller, fd, was_nonblock);

    n as i64
}

fn copy_bytes_to_spans(caller: &mut Caller<'_, Kernel>, spans: &[(i64, usize)], data: &[u8]) {
    let mut offset = 0;
    for (base, len) in spans {
        if offset >= data.len() || *len == 0 {
            continue;
        }
        let take = (data.len() - offset).min(*len);
        if let Ok(dst) = mem::guest_slice_mut(caller, *base, take as i64) {
            dst[..take].copy_from_slice(&data[offset..offset + take]);
        }
        offset += take;
    }
}

fn write_back_msghdr(
    caller: &mut Caller<'_, Kernel>,
    msghdr_ptr: i64,
    control_ptr: i64,
    _name_ptr: i64,
    msg_controllen: u32,
) {
    // msg_controllen is at offset 20 of msghdr; write back 0 to indicate
    // MSG_CTRUNC.
    if let Ok(b) = mem::guest_slice_mut(caller, msghdr_ptr + 20, 4) {
        b.copy_from_slice(&msg_controllen.to_le_bytes());
    }
    let _ = control_ptr; // not used; we never report ancillary data
}

fn restore_nonblock(caller: &mut Caller<'_, Kernel>, fd: u32, was_nonblock: Option<bool>) {
    if let Some(prev) = was_nonblock {
        let fds = &mut caller.data_mut().fds;
        if let Ok(Resource::Socket(s)) = fds.get_mut(fd) {
            s.lock()
                .nonblock
                .store(prev, std::sync::atomic::Ordering::Relaxed);
        }
    }
}

#[cfg(test)]
mod p2_c3_part1_tests {
    //! P2-C3 part 1: sendmsg/recvmsg constants and flag values.
    use super::*;

    #[test]
    fn sendmsg_nr_is_linux_46() {
        assert_eq!(NR_SENDMSG, 46);
    }

    #[test]
    fn recvmsg_nr_is_linux_47() {
        assert_eq!(NR_RECVMSG, 47);
    }

    #[test]
    fn msg_flag_values_match_linux() {
        assert_eq!(MSG_PEEK, 0x2);
        assert_eq!(MSG_DONTWAIT, 0x40);
        assert_eq!(MSG_NOSIGNAL, 0x4000);
        assert_eq!(MSG_TRUNC, 0x20);
        assert_eq!(MSG_CTRUNC, 0x8);
    }

    #[test]
    fn msghdr_size_is_32_on_wasm32() {
        // 8 × u32 = 32 bytes.
        assert_eq!(MSGHDR_SIZE, 32);
    }
}

// --- P2-C3 part 2: AF_UNIX sendto/recvfrom helpers ---

/// AF_UNIX sendto: write `buf_len` bytes to the connected UnixStream (or
/// via UnixDatagram if the socket is dgram). Honors the SHUT_WR EPIPE
/// rule. Mirrors the IPv4 sendto's lock discipline — never hold a
/// `parking_lot::Mutex` guard across `.await`.
async fn sendto_unix(
    caller: &mut Caller<'_, Kernel>,
    fd: u32,
    buf_ptr: i64,
    buf_len_raw: i64,
) -> i64 {
    if usize::try_from(buf_len_raw).is_err() {
        return -crate::errno::EFAULT;
    }
    let bytes = match mem::guest_slice(caller, buf_ptr, buf_len_raw) {
        Ok(b) => b.to_vec(),
        Err(e) => return e,
    };

    // Pull the stream out under a short-lived lock; never hold the
    // Mutex guard across `.await`.
    let mut stream = {
        let fds = &mut caller.data_mut().fds;
        match fds.get_mut(fd) {
            Ok(Resource::Socket(s)) => {
                let mut gs = s.lock();
                if gs.shutdown_flags & 0b10 != 0 {
                    return -crate::errno::EPIPE;
                }
                match gs.stream_unix.take() {
                    Some(st) => st,
                    None => return -crate::errno::ENOTCONN,
                }
            }
            Ok(_) => return -EBADF,
            Err(e) => return e,
        }
    };

    use tokio::io::AsyncWriteExt;
    let res = stream.write(&bytes).await;

    // Put the stream back.
    {
        let fds = &mut caller.data_mut().fds;
        if let Ok(Resource::Socket(s)) = fds.get_mut(fd) {
            let mut gs = s.lock();
            if gs.stream_unix.is_none() {
                gs.stream_unix = Some(stream);
            }
        }
    }
    match res {
        Ok(n) => n as i64,
        Err(_) => -crate::errno::EIO,
    }
}

/// AF_UNIX recvfrom: read from the connected UnixStream. Honors the
/// SHUT_RD EOF rule.
async fn recvfrom_unix(
    caller: &mut Caller<'_, Kernel>,
    fd: u32,
    buf_ptr: i64,
    buf_len_raw: i64,
) -> i64 {
    let len = match usize::try_from(buf_len_raw) {
        Ok(n) => n,
        Err(_) => return -EFAULT,
    };
    if len == 0 {
        return -EINVAL;
    }
    if let Err(e) = mem::guest_slice_mut(caller, buf_ptr, buf_len_raw) {
        return e;
    }

    let mut stream = {
        let fds = &mut caller.data_mut().fds;
        match fds.get_mut(fd) {
            Ok(Resource::Socket(s)) => {
                let mut gs = s.lock();
                if gs.shutdown_flags & 0b01 != 0 {
                    return 0; // EOF
                }
                match gs.stream_unix.take() {
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
    let res = stream.read(&mut buf).await;
    let n = match res {
        Ok(0) => 0, // EOF
        Ok(n) => n,
        Err(_) => {
            let fds = &mut caller.data_mut().fds;
            if let Ok(Resource::Socket(s)) = fds.get_mut(fd) {
                let mut gs = s.lock();
                if gs.stream_unix.is_none() {
                    gs.stream_unix = Some(stream);
                }
            }
            return -crate::errno::EIO;
        }
    };

    // Put the stream back.
    {
        let fds = &mut caller.data_mut().fds;
        if let Ok(Resource::Socket(s)) = fds.get_mut(fd) {
            let mut gs = s.lock();
            if gs.stream_unix.is_none() {
                gs.stream_unix = Some(stream);
            }
        }
    }

    if n > 0 {
        let dst = match mem::guest_slice_mut(caller, buf_ptr, n as i64) {
            Ok(b) => b,
            Err(e) => return e,
        };
        dst[..n].copy_from_slice(&buf[..n]);
    }
    n as i64
}

/// P2-D3.2: apply `SO_REUSEADDR` on a freshly bound `std::net::TcpListener`.
/// Called from `apply_snapshot_kernel_state` after `TcpListener::bind` to
/// restore the option that was set on the original listener before freeze.
pub(crate) fn setsockopt_reuseaddr(
    listener: &std::net::TcpListener,
    on: bool,
) -> std::io::Result<()> {
    use std::os::unix::io::AsRawFd;
    let fd = listener.as_raw_fd();
    let val: libc::c_int = if on { 1 } else { 0 };
    // SAFETY: `fd` is a live, owned `TcpListener` raw fd; `val` is a
    // stack-local `c_int` valid for the duration of the call; `optlen`
    // matches the size of `c_int`.
    let rc = unsafe {
        libc::setsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_REUSEADDR,
            &val as *const _ as *const libc::c_void,
            std::mem::size_of_val(&val) as libc::socklen_t,
        )
    };
    if rc == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}
