//! `poll(2)` — poll a set of fds for readiness.
//!
//! P2-B3: now async-suspending. Builds a `tokio::select!` over per-fd
//! readiness notifies (pipes have a `notify` field; sockets reuse the
//! P1-7 `notify_read`) plus `tokio::time::sleep(timeout)`. `timeout_ms`
//! < 0 omits the timer (wait indefinitely); 0 returns immediately.
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

use std::sync::Arc;

use tokio::sync::Notify;
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

/// `poll(fds_ptr, nfds, timeout_ms)`.
///
/// P2-B3: real async. Inspects the current snapshot of each fd; if any
/// has a non-zero revents matching `events`, returns immediately.
/// Otherwise builds a `tokio::select!` over per-fd readiness notifies
/// and a sleep timer. The select! body just falls through (the outer
/// loop re-checks the snapshot).
///
/// Returns: number of fds with non-zero `revents` (success), 0 on
/// timeout, negative errno on bad args.
pub async fn poll(caller: &mut Caller<'_, Kernel>, a: [i64; 6]) -> i64 {
    let fds_ptr = a[0];
    let nfds_raw = a[1];
    let timeout_ms = a[2];

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

    // Pre-validate the entire buffer up front.
    if let Err(e) = mem::guest_slice_mut(caller, fds_ptr, total) {
        return e;
    }

    // Snapshot the (fd, events) pairs upfront so we can iterate the
    // poll loop without re-reading guest memory on every wake.
    let entries: Vec<(i32, i16)> = {
        let mut out = Vec::with_capacity(nfds);
        for i in 0..nfds {
            let entry_ptr = fds_ptr + (i * POLLFD_SIZE) as i64;
            let entry = match mem::guest_slice(caller, entry_ptr, POLLFD_SIZE as i64) {
                Ok(b) => b,
                Err(_) => {
                    out.push((0, POLLNVAL));
                    continue;
                }
            };
            let fd = i32::from_le_bytes([entry[0], entry[1], entry[2], entry[3]]);
            let events = i16::from_le_bytes([entry[4], entry[5]]);
            out.push((fd, events));
        }
        out
    };

    // Collect the wake notifiers for the fds the guest is asking about.
    // (POLLNVAL/Epoll/EventFd fds don't get a wake source — they're
    // reported synchronously.)
    let mut wakes: Vec<Arc<Notify>> = Vec::new();
    {
        let fds_table: &FdTable = &caller.data().fds;
        for &(fd, _) in &entries {
            let fd_u = match u32::try_from(fd) {
                Ok(f) => f,
                Err(_) => continue,
            };
            match fds_table.get(fd_u) {
                Ok(Resource::PipeRead(p)) => wakes.push(p.notify.clone()),
                Ok(Resource::PipeWrite(p)) => wakes.push(p.notify.clone()),
                Ok(Resource::Socket(s)) => wakes.push(s.lock().notify_read.clone()),
                _ => {}
            }
        }
    }

    // Polling loop: re-snapshot readiness until something becomes ready
    // or the timer expires.
    let deadline = if timeout_ms == 0 {
        Some(tokio::time::Instant::now()) // already expired
    } else if timeout_ms > 0 {
        Some(tokio::time::Instant::now() + std::time::Duration::from_millis(timeout_ms as u64))
    } else {
        None
    };

    loop {
        // Snapshot readiness now.
        let readiness: Vec<i16> = {
            let fds_table: &FdTable = &caller.data().fds;
            entries
                .iter()
                .map(|&(fd, events)| poll_one(fds_table, fd, events))
                .collect()
        };
        // Anything ready (incl. POLLNVAL/POLLERR/POLLHUP) → write and return.
        if readiness.iter().any(|r| *r != 0) {
            return write_revents(caller, fds_ptr, &readiness);
        }
        // Timeout = 0 → we already checked, no wake; return 0.
        if timeout_ms == 0 {
            return write_revents(caller, fds_ptr, &readiness);
        }
        // Build the select! over wakes + (optional) timer.
        if let Some(dl) = deadline {
            let timeout_fut = tokio::time::sleep_until(dl);
            tokio::select! {
                biased;
                _ = timeout_fut => {
                    // Final snapshot — anything become ready just now?
                    let readiness: Vec<i16> = {
                        let fds_table: &FdTable = &caller.data().fds;
                        entries
                            .iter()
                            .map(|&(fd, events)| poll_one(fds_table, fd, events))
                            .collect()
                    };
                    return write_revents(caller, fds_ptr, &readiness);
                }
                _ = wait_any(&wakes) => {
                    // A wake fired; loop and re-check.
                    continue;
                }
            }
        } else {
            // No timeout → wait indefinitely on any wake.
            wait_any(&wakes).await;
            continue;
        }
    }
}

/// Wait until any of `wakes` fires. If `wakes` is empty, sleep forever.
async fn wait_any(wakes: &[Arc<Notify>]) {
    if wakes.is_empty() {
        // No fd to wait on; never wake. Matches Linux `poll` with no
        // valid fds and a -1 timeout (sleeps indefinitely).
        std::future::pending::<()>().await;
        return;
    }
    let futs: Vec<_> = wakes
        .iter()
        .map(|n| {
            Box::pin(n.notified())
                as std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + Sync>>
        })
        .collect();
    let (_winner, _idx, _rest) = futures::future::select_all(futs).await;
}

/// Write the readiness bits back into the guest buffer and return the
/// count of fds with non-zero revents.
fn write_revents(caller: &mut Caller<'_, Kernel>, fds_ptr: i64, readiness: &[i16]) -> i64 {
    let nfds = readiness.len();
    let total = (nfds * POLLFD_SIZE) as i64;
    let mut ready_count: i64 = 0;
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
        // P1-7: epoll/eventfd fds aren't useful as poll targets — report
        // POLLNVAL so the caller knows to use epoll_wait instead.
        Resource::Epoll(_) | Resource::EventFd(_) => POLLNVAL,
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

fn ready_socket(
    events: i16,
    s: &crate::fd::SharedSocket,
) -> i16 {
    let mut r: i16 = 0;
    // Stream sockets: connected ⇒ always POLLIN-ready (the recvfrom will
    // await real data via the lazy TcpStream). For a listener, no socket
    // data is available — that's poll(POLLOUT)'s domain.
    let gs = s.lock();
    let is_listener = gs.is_listening();
    let has_stream = gs.stream.is_some();
    let has_bound = gs.bound.is_some();
    let is_v4 = matches!(gs.bound, Some(SockAddr::V4 { .. }) | None);
    let _ = is_v4; // silence unused warning on V6 builds
    drop(gs);

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
        let fd = fds.insert(Resource::Socket(std::sync::Arc::new(
            parking_lot::Mutex::new({
                let mut s = crate::fd::SocketInner::new(crate::fd::SocketKind::Stream, false);
                s.bound = Some(crate::fd::SockAddr::V4 {
                    port: 8080,
                    addr: [127, 0, 0, 1],
                });
                s.listen_backlog = Some(5);
                s
            }),
        )));
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