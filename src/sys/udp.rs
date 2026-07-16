//! Path A UDP socket layer (ADR 0008).
//!
//! This module owns the host-side UDP state — a `UdpSocket` from tokio
//! plus a bounded recv queue fed by a background pump task, and the
//! shared `Arc<Notify>` machinery that wake `poll` / `epoll_wait`
//! callers.
//!
//! The data-path handlers in `src/sys/socket.rs` (C1..C4) construct a
//! `UdpSocketState` on the first `bind`/`connect`/`sendto`/`recvfrom`
//! that hits a UDP fd, store it on `SocketInner.udp`, and then move
//! ownership of the host handle into the pump task on first recv.
//!
//! ## Lock discipline (carries from ADR 0001 §2 / 0006 §4)
//!
//! - All `Arc<…>` fields on `UdpSocketState` are clone-while-locked,
//!   drop-guard, then await — **never hold a parking_lot guard across
//!   `.await`** (this is enforced by CI: `scripts/check_no_fragile_unwraps.sh`
//!   + `clippy::await_holding_lock`).
//! - The pump task owns the `Arc<UdpSocket>` exclusively; poll/epoll
//!   observe readiness via the `Arc<Notify>` and the `pending` flag.
//!
//! ## Snapshot non-persistence
//!
//! `UdpSocketState` is **not** serialized into `KernelSnapshot` — only
//! the bind/peer/sockopt *metadata* is. On apply the host handle is
//! rebuilt fresh via `UdpSocket::bind` (see ADR 0008 §Snapshot +
//! `src/snapshot.rs` apply path, which lands in C7). The pump task,
//! the recv queue, and any Notify waiters are deliberately dropped.

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use parking_lot::Mutex;
use tokio::net::UdpSocket;
use tokio::sync::Notify;
use tokio::task::JoinHandle;

/// One queued inbound datagram. Pushed by the pump task, drained by
/// `recvfrom`/`recvmsg` handlers.
#[derive(Debug)]
pub struct Datagram {
    pub bytes: Vec<u8>,
    pub addr: SocketAddr,
}

/// Bounded recv queue shared between the pump task (writer) and the
/// `recvfrom`/`recvmsg` handlers (reader). `u16` cap is more than
/// enough for any realistic DNS workload; if it ever overflows, the
/// pump drops the oldest packet (lossy, documented).
const RECV_QUEUE_CAP: usize = 4096;

/// Address family tag for the bound socket. Used to decide whether to
/// apply `IPV6_V6ONLY` at bind time and to map V4 destinations over a
/// dual-stack V6 socket.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Family {
    V4,
    V6,
}

/// The host-side state for a UDP fd. Owned by `SocketInner.udp`. Once
/// the host `UdpSocket` is materialized and the pump task is spawned,
/// both are reachable only through `Arc` — the lock-then-clone-then-
/// drop discipline lets handlers await without holding a guard.
///
/// C0 lands this struct with the field layout and the in/out methods;
/// the bind/send/recv/pump bodies land in C1..C5.
#[allow(dead_code)]
pub struct UdpSocketState {
    /// Host UDP socket. Lazily set on first bind/connect/sendto/recvfrom
    /// (whichever comes first). Once `Some`, owned by the pump task
    /// for reads; send_to/connect may take a clone of the inner Arc.
    pub socket: Mutex<Option<Arc<UdpSocket>>>,

    /// Bounded recv queue (pump task → handler).
    pub recv_queue: Mutex<VecDeque<Datagram>>,

    /// Fires when a packet arrives (and the queue was empty before).
    /// poll/epoll subscribe via the same `Arc<Notify>` machinery as TCP.
    pub notify_read: Arc<Notify>,

    /// Fires when the socket becomes writable (always immediately for
    /// UDP after bind, but modeled for symmetry with TCP).
    pub notify_write: Arc<Notify>,

    /// Bound local address (set by `bind`; updated by the kernel with
    /// the actual ephemeral port after `bind(0.0.0.0:0)`).
    pub bound_addr: Mutex<Option<SocketAddr>>,

    /// Peer address set by `connect`; `None` for connectionless UDP.
    /// `sendto` with explicit addr ignores this.
    pub peer_addr: Mutex<Option<SocketAddr>>,

    /// Address family of the host socket. Determines `IPV6_V6ONLY`
    /// handling at bind time.
    pub family: Family,

    /// `IPV6_V6ONLY` requested via setsockopt (only meaningful for V6).
    pub ipv6_v6only: bool,

    /// SO_REUSEADDR requested via setsockopt. Applied pre-bind.
    pub so_reuseaddr: bool,

    /// Shutdown flags — bit 0 = SHUT_RD, bit 1 = SHUT_WR. Linux UDP
    /// supports shutdown; we mirror it on the recv/send paths.
    pub shutdown_flags: Mutex<u8>,

    /// Pump task handle. `Some` after the first recvfrom (or C5 pump
    /// spawn). C5 lands the actual `tokio::spawn` body.
    pub pump_handle: Mutex<Option<JoinHandle<()>>>,

    /// Pump cancellation flag. Set by `Drop` and by the explicit close
    /// path. The pump polls this between awaits.
    pub pump_cancel: Arc<AtomicBool>,
}

#[allow(dead_code)]
impl UdpSocketState {
    /// Construct an empty `UdpSocketState`. The host socket is created
    /// lazily on the first bind/sendto/recvfrom.
    pub fn new(family: Family, ipv6_v6only: bool, so_reuseaddr: bool) -> Self {
        Self {
            socket: Mutex::new(None),
            recv_queue: Mutex::new(VecDeque::with_capacity(64)),
            notify_read: Arc::new(Notify::new()),
            notify_write: Arc::new(Notify::new()),
            bound_addr: Mutex::new(None),
            peer_addr: Mutex::new(None),
            family,
            ipv6_v6only,
            so_reuseaddr,
            shutdown_flags: Mutex::new(0),
            pump_handle: Mutex::new(None),
            pump_cancel: Arc::new(AtomicBool::new(false)),
        }
    }

    /// True if the recv queue has at least one packet waiting.
    /// Called from `poll`/`epoll_wait` `ready_socket` under a brief
    /// lock; the `clippy::await_holding_lock` lint is the load-bearing
    /// enforcement for the callers of this method.
    pub fn has_pending(&self) -> bool {
        let q = self.recv_queue.lock();
        !q.is_empty()
    }

    /// Pop one datagram from the recv queue (FIFO). Returns `None`
    /// when the queue is empty.
    pub fn try_dequeue(&self) -> Option<Datagram> {
        let mut q = self.recv_queue.lock();
        q.pop_front()
    }

    /// Push one datagram into the recv queue. Drops the oldest packet
    /// if the queue is full (bounded by `RECV_QUEUE_CAP`).
    /// Returns true if the queue was empty before this push (so the
    /// caller can decide whether to fire `notify_read`).
    pub fn enqueue(&self, bytes: Vec<u8>, addr: SocketAddr) -> bool {
        let mut q = self.recv_queue.lock();
        let was_empty = q.is_empty();
        if q.len() >= RECV_QUEUE_CAP {
            q.pop_front();
        }
        q.push_back(Datagram { bytes, addr });
        was_empty
    }

    /// Mark the pump task for cancellation. The pump polls this between
    /// awaits and exits cleanly. Idempotent.
    pub fn request_pump_cancel(&self) {
        self.pump_cancel.store(true, Ordering::Relaxed);
        // Also wake any recvfrom/recvmsg currently parked on notify_read
        // so it can observe the cancellation and return -EINTR or similar.
        self.notify_read.notify_waiters();
    }

    // ----- C1: bind helpers -----

    /// Materialize the host `UdpSocket` from `socket2` + tokio. Idempotent:
    /// if a socket is already installed, returns the existing `Arc`.
    ///
    /// Pre-bind setsockopts applied here:
    /// - `SO_REUSEADDR` (if `so_reuseaddr`)
    /// - `IPV6_V6ONLY` (if family == V6)
    ///
    /// The bind call is **sync** — `tokio::net::UdpSocket::from_std`
    /// only takes ownership; it does not `.await`. Bind itself is
    /// `socket2::Socket::bind` which is also sync. So no parking_lot
    /// guards need to be held across awaits on this path.
    ///
    /// Errors map:
    /// - `AddrInUse` → `-EADDRINUSE`
    /// - `AddrNotAvail` → `-EADDRNOTAVAIL`
    /// - anything else → `-EIO`
    pub fn ensure_bound(&self, requested: SocketAddr) -> Result<std::net::SocketAddr, i64> {
        // Fast path: already bound.
        if let Some(sock) = self.socket.lock().as_ref() {
            // local_addr() is sync and always succeeds on a bound socket.
            return sock.local_addr().map_err(|_| -crate::errno::EIO);
        }

        let domain = match self.family {
            Family::V4 => socket2::Domain::IPV4,
            Family::V6 => socket2::Domain::IPV6,
        };
        let sock_type = socket2::Type::DGRAM;
        // protocol=0 — let the OS pick the right IPPROTO_UDP for the family.
        let raw =
            socket2::Socket::new(domain, sock_type, Some(socket2::Protocol::UDP)).map_err(|e| {
                match e.kind() {
                    std::io::ErrorKind::PermissionDenied => -crate::errno::EACCES,
                    _ => -crate::errno::EIO,
                }
            })?;

        // SO_REUSEADDR pre-bind (matches Linux semantics; harmless when
        // the guest didn't ask for it).
        if self.so_reuseaddr {
            let _ = raw.set_reuse_address(true);
        }

        // V6-only flag — only meaningful on AF_INET6; ignored on V4.
        if matches!(self.family, Family::V6) {
            // IPV6_V6ONLY is the integer 26 on Linux. socket2 doesn't
            // expose a typed setter for it; use setsockopt_int.
            let _ = raw.set_only_v6(self.ipv6_v6only);
        }

        // Bind. Note: a V4 destination address passed to an AF_INET6
        // socket will be mapped by the kernel to ::ffff:a.b.c.d; we
        // accept that here and let the OS handle it (Linux does too).
        raw.bind(&socket2::SockAddr::from(requested))
            .map_err(|e| match e.kind() {
                std::io::ErrorKind::AddrInUse => -crate::errno::EADDRINUSE,
                std::io::ErrorKind::AddrNotAvailable => -crate::errno::EADDRNOTAVAIL,
                std::io::ErrorKind::PermissionDenied => -crate::errno::EACCES,
                _ => -crate::errno::EIO,
            })?;

        // Convert to a tokio socket. `from_std` does NOT register
        // interest in the runtime's IO driver — it only wraps the fd.
        // For UDP, `recv_from` lazily registers on first call (C2).
        let std_sock: std::net::UdpSocket = raw.into();
        std_sock
            .set_nonblocking(true)
            .map_err(|_| -crate::errno::EIO)?;
        let tokio_sock =
            tokio::net::UdpSocket::from_std(std_sock).map_err(|_| -crate::errno::EIO)?;
        let bound_addr = tokio_sock.local_addr().map_err(|_| -crate::errno::EIO)?;

        let arc = Arc::new(tokio_sock);
        *self.socket.lock() = Some(arc);
        *self.bound_addr.lock() = Some(bound_addr);
        Ok(bound_addr)
    }

    /// Read the current `bound_addr`. Returns `None` until `ensure_bound`
    /// has been called.
    pub fn local_addr(&self) -> Option<SocketAddr> {
        *self.bound_addr.lock()
    }
}

impl Drop for UdpSocketState {
    fn drop(&mut self) {
        // Signal the pump task to exit (if any). The JoinHandle is
        // detached — the task exits on its own within one await cycle.
        // We do NOT block on the join here: Drop is sync, and the
        // pump task may be parked in `udp.readable().await`. The task
        // observes `pump_cancel` and exits cleanly.
        self.pump_cancel.store(true, Ordering::Relaxed);
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn udp_socket_state_default_has_no_socket_no_queue() {
        let s = UdpSocketState::new(Family::V4, false, false);
        assert!(!s.has_pending());
        assert!(s.try_dequeue().is_none());
        assert!(s.bound_addr.lock().is_none());
        assert!(s.peer_addr.lock().is_none());
    }

    #[test]
    fn udp_socket_state_enqueue_returns_true_when_was_empty() {
        let s = UdpSocketState::new(Family::V4, false, false);
        assert!(s.enqueue(vec![1, 2, 3], "127.0.0.1:53".parse().unwrap()));
        assert!(s.has_pending());
        // Second push — queue not empty, so returns false.
        assert!(!s.enqueue(vec![4, 5, 6], "127.0.0.1:53".parse().unwrap()));
    }

    #[test]
    fn udp_socket_state_dequeue_is_fifo() {
        let s = UdpSocketState::new(Family::V4, false, false);
        let a: SocketAddr = "127.0.0.1:53".parse().unwrap();
        let b: SocketAddr = "127.0.0.1:54".parse().unwrap();
        s.enqueue(vec![1], a);
        s.enqueue(vec![2], b);
        let d1 = s.try_dequeue().unwrap();
        assert_eq!(d1.bytes, vec![1]);
        assert_eq!(d1.addr, a);
        let d2 = s.try_dequeue().unwrap();
        assert_eq!(d2.bytes, vec![2]);
        assert_eq!(d2.addr, b);
        assert!(s.try_dequeue().is_none());
    }

    #[test]
    fn udp_socket_state_queue_overflow_drops_oldest() {
        let s = UdpSocketState::new(Family::V4, false, false);
        let a: SocketAddr = "127.0.0.1:53".parse().unwrap();
        // Push RECV_QUEUE_CAP + 1 packets. The first must be dropped.
        let total = RECV_QUEUE_CAP + 1;
        for i in 0..total {
            s.enqueue(vec![i as u8], a);
        }
        // Drain — the first packet should be `1`, not `0`.
        let first = s.try_dequeue().unwrap();
        assert_eq!(first.bytes, vec![1]);
    }

    #[test]
    fn udp_socket_state_pump_cancel_is_idempotent() {
        let s = UdpSocketState::new(Family::V4, false, false);
        s.request_pump_cancel();
        s.request_pump_cancel();
        assert!(s.pump_cancel.load(Ordering::Relaxed));
    }

    #[test]
    fn udp_socket_state_shutdown_flags_default_zero() {
        let s = UdpSocketState::new(Family::V4, false, false);
        assert_eq!(*s.shutdown_flags.lock(), 0);
    }
}
