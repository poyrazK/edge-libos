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

use crate::errno::{EAFNOSUPPORT, EPROTONOSUPPORT};
use crate::fd::{Resource, SocketInner, SocketKind};
use crate::kernel::Kernel;

// NR_* (Linux x86-64 unistd_64.h).
pub const NR_SOCKET: u32 = 41;

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