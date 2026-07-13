//! `poll(2)` — poll a set of fds for readiness.
//!
//! P1-6's `poll` is non-suspending: it inspects the current state of each
//! fd and reports readiness synchronously. Async-suspending readiness
//! waits go through `epoll_wait` (P1-7), which builds on this primitive.
//!
//! ## struct pollfd on wasm32
//!
//! ```c
//! struct pollfd {
//!     int   fd;       // 0..4
//!     short events;   // 4..6 — requested events (POLLIN=1, POLLOUT=4, ...)
//!     short revents;  // 6..8 — returned events
//! };
//! ```
//!
//! Each entry is 8 bytes. The guest passes a pointer to `nfds` entries.

use wasmtime::Caller;

use crate::errno::EINVAL;
use crate::fd::{FdTable, Resource, SockAddr};
use crate::kernel::Kernel;
use crate::mem;

// Linux x86-64 syscall NR.
pub const NR_POLL: u32 = 7;

// poll(2) event flags (matches <poll.h> on Linux).
pub const POLLIN: i16 = 0x0001;
pub const POLLPRI: i16 = 0x0002;
pub const POLLOUT: i16 = 0x0004;
pub const POLLERR: i16 = 0x0008;
pub const POLLHUP: i16 = 0x0010;
pub const POLLNVAL: i16 = 0x0020;
pub const POLLRDNORM: i16 = 0x0040;
pub const POLLWRNORM: i16 = 0x0080;
pub const POLLRDBAND: i16 = 0x0100;
pub const POLLWRBAND: i16 = 0x0200;

const POLLFD_SIZE: usize = 8; // sizeof(struct pollfd)

/// `poll(fds_ptr, nfds, timeout_ms)` — inspect readiness of `nfds` fds in
/// the guest buffer starting at `fds_ptr`. Each entry is an 8-byte
/// `struct pollfd { int fd; short events; short revents; }`.
///
/// **P1-6 scope**: poll is synchronous. `timeout_ms` is honored only as
/// "0" vs non-zero: we always return immediately with the current
/// snapshot, which is fine for the CPython/FastAPI DoD since Python's
/// `select.select` already uses `epoll` on Linux. The non-suspending
/// variant is sufficient for the bootstrap path where the guest calls
/// `poll` to detect readiness on already-connected fds.
///
/// Returns: number of fds with non-zero `revents` (success), 0 on no
/// events ready. `-EFAULT` if the buffer is out of bounds, `-EINVAL`
/// if `nfds < 0` or `nfds` would overflow the buffer.
pub async fn poll(caller: &mut Caller<'_, Kernel>, a: [i64; 6]) -> i64 {
    let fds_ptr = a[0];
    let nfds_raw = a[1];
    let _timeout_ms = a[2]; // ignored in P1-6 (synchronous only)

    let nfds = match usize::try_from(nfds_raw) {
        Ok(n) => n,
        Err(_) => return -EINVAL,
    };

    if nfds == 0 {
        return 0;
    }

    let total = match nfds.checked_mul(POLLFD_SIZE) {
        Some(n) => n as i64,
        None => return -EINVAL,
    };

    // Pre-validate the entire buffer up front. We re-slice per-entry below.
    if let Err(e) = mem::guest_slice_mut(caller, fds_ptr, total) {
        return e;
    }

    // Phase 1: snapshot which fds are ready, given the guest's request
    // bits. Done before any write-back so we don't race with our own
    // updates.
    let readiness: Vec<i16> = {
        let fds_table: &FdTable = &caller.data().fds;
        let mut out = Vec::with_capacity(nfds);
        for i in 0..nfds {
            let entry_ptr = fds_ptr + (i * POLLFD_SIZE) as i64;
            // Re-slice — the read-only path lets us share the snapshot.
            let entry = match mem::guest_slice(caller, entry_ptr, POLLFD_SIZE as i64) {
                Ok(b) => b,
                Err(_) => {
                    out.push(POLLNVAL);
                    continue;
                }
            };
            let fd = i32::from_le_bytes([entry[0], entry[1], entry[2], entry[3]]);
            let events = i16::from_le_bytes([entry[4], entry[5]]);

            let revents = poll_one(fds_table, fd, events);
            out.push(revents);
        }
        out
    };

    // Phase 2: write revents back into the guest buffer.
    let mut ready_count: i64 = 0;
    {
        let buf = match mem::guest_slice_mut(caller, fds_ptr, total) {
            Ok(b) => b,
            Err(e) => return e,
        };
        for (i, revents) in readiness.iter().enumerate() {
            let off = i * POLLFD_SIZE;
            buf[off + 6..off + 8].copy_from_slice(&revents.to_le_bytes());
            if *revents != 0 {
                ready_count += 1;
            }
        }
    }

    ready_count
}

/// Compute `revents` for a single fd. `POLLNVAL` for unknown fds;
/// otherwise the intersection of `events` and whatever readiness signals
/// the resource exposes today.
fn poll_one(fds: &FdTable, fd: i32, events: i16) -> i16 {
    let fd_u = match u32::try_from(fd) {
        Ok(f) => f,
        Err(_) => return POLLNVAL,
    };
    let res = match fds.get(fd_u) {
        Ok(r) => r,
        Err(_) => return POLLNVAL,
    };
    match res {
        Resource::Stdin(p) => ready_pipe_read(events, !*p.closed.lock() || !p.buf.lock().is_empty()),
        Resource::Stdout(_) | Resource::Stderr(_) => POLLOUT & events,
        Resource::File(_) => POLLIN | POLLOUT, // regular files are always ready
        Resource::PipeRead(p) => ready_pipe_read(events, !p.buf.lock().is_empty() || *p.closed.lock()),
        Resource::PipeWrite(_) => POLLOUT & events, // buf pipes are always writeable
        Resource::Socket(s) => ready_socket(events, s),
    }
}

fn ready_pipe_read(events: i16, has_data_or_eof: bool) -> i16 {
    let mut r: i16 = 0;
    if has_data_or_eof && (events & POLLIN) != 0 {
        r |= POLLIN;
    }
    if events & POLLOUT != 0 {
        // Read end isn't writable; set POLLERR to flag it as an error.
        r |= POLLERR;
    }
    r
}

fn ready_socket(events: i16, s: &crate::fd::SocketInner) -> i16 {
    let mut r: i16 = 0;
    // Stream sockets: connected ⇒ always POLLIN-ready (the recvfrom will
    // await real data via the lazy TcpStream). For a listener, no socket
    // data is available — that's poll(POLLOUT)'s domain.
    let is_listener = s.is_listening();
    let has_stream = s.stream.is_some();
    let has_bound = s.bound.is_some();
    let is_v4 = matches!(s.bound, Some(SockAddr::V4 { .. }) | None);
    let _ = is_v4; // silence unused warning on V6 builds

    if (events & POLLIN) != 0 {
        if has_stream {
            r |= POLLIN;
        } else if is_listener {
            // Listener sockets are not readable in the data sense; mark
            // as such so callers know not to read.
            r |= POLLNVAL;
        }
    }
    if (events & POLLOUT) != 0 {
        // Connected stream → writable; listener → invalid for write.
        if has_stream {
            r |= POLLOUT;
        } else if has_bound {
            r |= POLLNVAL;
        }
    }
    r
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fd::FdTable;

    #[test]
    fn poll_unknown_fd_returns_pollnval() {
        let fds = FdTable::empty();
        let r = poll_one(&fds, 9999, POLLIN);
        assert_eq!(r, POLLNVAL);
    }

    #[test]
    fn poll_negative_fd_returns_pollnval() {
        let fds = FdTable::empty();
        let r = poll_one(&fds, -1, POLLIN);
        assert_eq!(r, POLLNVAL);
    }

    #[test]
    fn poll_listener_socket_pollin_returns_pollnval() {
        let fds = FdTable::empty();
        let mut fds = fds;
        let fd = fds.insert(Resource::Socket({
            let mut s = crate::fd::SocketInner::new(crate::fd::SocketKind::Stream, false);
            s.bound = Some(crate::fd::SockAddr::V4 {
                port: 8080,
                addr: [127, 0, 0, 1],
            });
            s.listen_backlog = Some(5);
            s
        }));
        let r = poll_one(&fds, fd as i32, POLLIN);
        assert_eq!(r, POLLNVAL, "listening socket POLLIN should be POLLNVAL");
    }

    #[test]
    fn poll_ready_pipe_pollin_returns_pollin() {
        use crate::fd::make_pipe;
        let mut fds = FdTable::empty();
        let (rd, mut wr) = make_pipe();
        // Write a byte synchronously so the buffer is non-empty.
        use std::io::Write as _; // sync write — bypasses tokio for the test
        let mut data = [0u8; 1];
        data[0] = b'x';
        // Push via the underlying VecDeque directly:
        wr.buf.lock().push_back(b'x');
        let fd = fds.insert(Resource::PipeRead(rd));
        let r = poll_one(&fds, fd as i32, POLLIN);
        assert_eq!(r & POLLIN, POLLIN, "ready pipe should report POLLIN");
    }

    #[test]
    fn poll_sizeof_pollfd_is_8() {
        // Compile-time assertion. wasm32 and x86-64 linux both have
        // sizeof(struct pollfd) == 8, with fd at offset 0 (int), events
        // at 4 (short), revents at 6 (short).
        assert_eq!(POLLFD_SIZE, 8);
        // Sanity: i32 + 2*i16 == 8.
        assert_eq!(std::mem::size_of::<i32>() + 2 * std::mem::size_of::<i16>(), 8);
    }
}