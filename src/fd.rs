//! Per-process file-descriptor table.
//!
//! P0 covers stdin / stdout / stderr only. Each stdio fd is backed by a
//! `BufPipe`-style buffer that lives in the kernel; tests can construct a
//! Kernel with their own buffers, the binary driver uses the host stdio.
//!
//! The `Resource` enum fills in as VFS lands (File, Dir, PipeRead/Write,
//! eventually Socket/Epoll in P1).

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicI32};
use std::sync::Arc;

use parking_lot::Mutex;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::{TcpListener, TcpStream, UnixDatagram, UnixListener, UnixStream};

pub const AT_FDCWD: i64 = -100;
pub const STDIN: u32 = 0;
pub const STDOUT: u32 = 1;
pub const STDERR: u32 = 2;

/// Read end of an in-kernel byte buffer. Implements `AsyncRead`.
pub struct PipeRead {
    pub buf: Arc<Mutex<VecDeque<u8>>>,
    pub closed: Arc<Mutex<bool>>,
    /// P1-3: `O_NONBLOCK` flag, honored by `read` in `crate::sys::file`.
    pub nonblock: Arc<AtomicBool>,
    /// P2-B3: fires when the pipe read-side becomes ready (data
    /// available, or close). `poll` subscribes to this for async
    /// readiness waits.
    pub notify: Arc<tokio::sync::Notify>,
}

impl AsyncRead for PipeRead {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        let mut inner = self.buf.lock();
        if *self.closed.lock() && inner.is_empty() {
            return std::task::Poll::Ready(Ok(())); // EOF
        }
        if inner.is_empty() {
            cx.waker().wake_by_ref();
            return std::task::Poll::Pending;
        }
        let n = buf.remaining().min(inner.len());
        for dst in buf.initialize_unfilled_to(n).iter_mut() {
            *dst = inner.pop_front().unwrap();
        }
        buf.advance(n);
        std::task::Poll::Ready(Ok(()))
    }
}

/// Write end of an in-kernel byte buffer. Implements `AsyncWrite`.
pub struct PipeWrite {
    pub buf: Arc<Mutex<VecDeque<u8>>>,
    pub closed: Arc<Mutex<bool>>,
    /// P1-3: `O_NONBLOCK` flag. Buffer pipes always accept writes today
    /// (the buffer is unbounded), so this is recorded for fidelity but
    /// does not currently affect `write` semantics.
    pub nonblock: Arc<AtomicBool>,
    /// P2-B3: fires when bytes are pushed onto the pipe (wakes any
    /// `poll` waiting for POLLIN on the read side).
    pub notify: Arc<tokio::sync::Notify>,
}

impl AsyncWrite for PipeWrite {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
        src: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        let mut inner = self.buf.lock();
        inner.extend(src.iter().copied());
        std::task::Poll::Ready(Ok(src.len()))
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::task::Poll::Ready(Ok(()))
    }

    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        *self.closed.lock() = true;
        std::task::Poll::Ready(Ok(()))
    }
}

/// Construct a paired (read, write) buffer-backed pipe.
pub fn make_pipe() -> (PipeRead, PipeWrite) {
    let buf = Arc::new(Mutex::new(VecDeque::new()));
    let closed = Arc::new(Mutex::new(false));
    let nonblock = Arc::new(AtomicBool::new(false));
    let notify_rd = Arc::new(tokio::sync::Notify::new());
    let notify_wr = Arc::new(tokio::sync::Notify::new());
    (
        PipeRead {
            buf: buf.clone(),
            closed: closed.clone(),
            nonblock: nonblock.clone(),
            notify: notify_rd,
        },
        PipeWrite {
            buf,
            closed,
            nonblock,
            notify: notify_wr,
        },
    )
}

/// What kind of socket this is. P1-1 only allocates the resource; the
/// stream-vs-datagram distinction matters once `connect`/`sendto` land.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SocketKind {
    Stream,
    Datagram,
}

/// The bound address of a socket. Parsed from `sockaddr_in` / `sockaddr_in6`
/// at `bind()` time, then stored on the `SocketInner` for use by `listen()`
/// (P1-2) and `accept4` (P1-4). For now we only model IPv4; IPv6 lands with
/// the listener work in P1-4 since they share the lazy-build path.
///
/// P2-C3 part 2: `Unix` variant for AF_UNIX filesystem-path sockets.
/// Abstract namespace (`sun_path[0] == 0`) → `-EOPNOTSUPP`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SockAddr {
    V4 { port: u16, addr: [u8; 4] },
    V6 { port: u16, addr: [u8; 16] },
    Unix { path: PathBuf },
}

impl SockAddr {
    /// Build a `SocketAddrV4` from a `SockAddr::V4`. Returns `None` for V6
    /// (handled separately when we add IPv6 listener support) and Unix.
    pub fn as_v4(&self) -> Option<std::net::SocketAddrV4> {
        match self {
            SockAddr::V4 { port, addr } => {
                std::net::SocketAddrV4::new(std::net::Ipv4Addr::from(*addr), *port).into()
            }
            SockAddr::V6 { .. } | SockAddr::Unix { .. } => None,
        }
    }
}

/// A freshly-created socket fd (P1-1). No connection state yet.
///
/// Future sub-steps will add fields here:
///   P1-2: bound `SockAddr` + lazy `tokio::net::TcpListener`
///   P1-5: connected `tokio::net::TcpStream`
///   P1-7: `notify_read`/`notify_write` for epoll integration
#[allow(dead_code)]
pub struct SocketInner {
    pub kind: SocketKind,
    pub nonblock: AtomicBool,
    /// P1-2: set by `bind()`. Until this is `Some`, the socket has no address.
    pub bound: Option<SockAddr>,
    /// P1-2: set by `listen()`. Until this is `Some`, the socket is not
    /// passive and `accept4` (P1-4) will return -EINVAL.
    pub listen_backlog: Option<i32>,
    /// P1-3: SO_REUSEADDR requested via `setsockopt`. Recorded for
    /// fidelity; surfaced on the lazy TcpListener in P1-7.
    pub so_reuseaddr: bool,
    /// P1-3: SO_KEEPALIVE requested via `setsockopt`. Recorded for
    /// fidelity; surfaced on the lazy TcpStream in P1-5/P1-7.
    pub so_keepalive: bool,
    /// P1-3: TCP_NODELAY requested via `setsockopt`. Recorded for
    /// fidelity; surfaced on the lazy TcpStream in P1-5/P1-7.
    pub tcp_nodelay: bool,
    /// P1-4: lazy tokio::net::TcpListener materialized on the first
    /// `accept4` call. Built from `bound` + `so_reuseaddr`.
    pub listener: Option<TcpListener>,
    /// P1-4: for accepted sockets — the connection's TcpStream. Set by
    /// `accept4`, used by `recvfrom`/`sendto`/`close` (P1-5) and by
    /// `epoll_wait` (P1-7).
    pub stream: Option<TcpStream>,
    /// P1-6: peer's `SocketAddr` as observed by the kernel. Set by
    /// `accept4` (peer = host connect) and `connect` (peer = listener addr).
    /// Read by `getpeername` to write back into the guest.
    pub peer_addr: Option<std::net::SocketAddr>,
    /// P1-6: last error from async connect, read by `getsockopt(SO_ERROR)`.
    /// 0 means "no pending error". Linux clears the error after read.
    pub last_error: AtomicI32,
    /// P1-6: shutdown flags — bit 0 = SHUT_RD, bit 1 = SHUT_WR.
    /// Once set, reads/writes on the underlying stream return EOF/EPIPE.
    pub shutdown_flags: u8,
    /// P1-6: is this socket a listener? Set on first `accept4` materialization.
    /// Surfaced via `getsockopt(SO_ACCEPTCONN)`.
    pub is_acceptor: bool,
    /// P1-7: Notify that fires when the socket's *read-side* readiness
    /// changes (data arrives, peer connects, half-close from peer).
    /// epoll_wait subscribes to this to wake when a watched fd becomes
    /// readable.
    pub notify_read: Arc<tokio::sync::Notify>,
    /// P1-7: Notify that fires when the *write-side* readiness changes
    /// (peer accepted, buffer drained). epoll_wait subscribes to this
    /// for `EPOLLOUT` watchers.
    pub notify_write: Arc<tokio::sync::Notify>,
    /// P2-C3: PEEK buffer for `recvmsg(MSG_PEEK)`. Bytes peeked from the
    /// stream are stashed here; subsequent non-peek reads drain this
    /// queue first. Lock briefly when accessing; never hold a Mutex
    /// guard across `.await`.
    pub peek_buf: parking_lot::Mutex<VecDeque<u8>>,
    /// P2-C3 part 2: AF_UNIX host state. `Some(_)` only when
    /// `family_unix == true`. Read/write briefly under the same lock as
    /// the rest of `SocketInner` (it's a `Mutex<SocketInner>`).
    pub unix: Option<UnixSockInner>,
    /// P2-C3 part 2: this socket is an AF_UNIX socket. Recorded at
    /// `socket(AF_UNIX, ...)` time so dispatchers can branch on family
    /// without re-parsing `bound`.
    pub family_unix: bool,
    /// P2-C3 part 2: connected UnixStream (post-`accept4` or
    /// post-`connect`). Mirrors `stream` for IPv4. Held outside
    /// `unix` for direct access without the inner `Option`.
    pub stream_unix: Option<UnixStream>,
    /// P2-C3 part 2: peer address for an AF_UNIX accepted/connected
    /// stream. Mirrors `peer_addr` for IPv4.
    pub peer_addr_unix: Option<std::os::unix::net::SocketAddr>,
    /// P2-C3 part 2: AF_UNIX datagram socket. Lazily bound on first
    /// sendto/recvfrom (the bind step is explicit in CPython; we
    /// support `bind(AF_UNIX, SOCK_DGRAM)` separately).
    pub dgram_unix: Option<UnixDatagram>,
}

impl SocketInner {
    pub fn new(kind: SocketKind, nonblock: bool) -> Self {
        Self {
            kind,
            nonblock: AtomicBool::new(nonblock),
            bound: None,
            listen_backlog: None,
            so_reuseaddr: false,
            so_keepalive: false,
            tcp_nodelay: false,
            listener: None,
            stream: None,
            peer_addr: None,
            last_error: AtomicI32::new(0),
            shutdown_flags: 0,
            is_acceptor: false,
            notify_read: Arc::new(tokio::sync::Notify::new()),
            notify_write: Arc::new(tokio::sync::Notify::new()),
            peek_buf: parking_lot::Mutex::new(VecDeque::new()),
            unix: None,
            family_unix: false,
            stream_unix: None,
            peer_addr_unix: None,
            dgram_unix: None,
        }
    }

    /// P2-C3 part 2: construct an AF_UNIX SocketInner.
    pub fn new_unix(kind: SocketKind, nonblock: bool) -> Self {
        let mut s = Self::new(kind, nonblock);
        s.unix = Some(UnixSockInner::new());
        s.family_unix = true;
        s
    }

    /// True once `bind` + `listen` have both run. P1-4 `accept4` requires this.
    #[allow(dead_code)]
    pub fn is_listening(&self) -> bool {
        self.bound.is_some() && self.listen_backlog.is_some()
    }

    /// P1-4: construct a SocketInner for a freshly accepted connection.
    #[allow(dead_code)]
    pub fn from_accepted(stream: TcpStream, kind: SocketKind, nonblock: bool) -> Self {
        let mut s = Self::new(kind, nonblock);
        s.stream = Some(stream);
        s
    }

    /// P1-6: family inferred from `bound` (or AF_INET if unknown). For
    /// AF_UNIX sockets the `family_unix` flag is authoritative regardless
    /// of whether `bound` is set.
    pub fn family(&self) -> i32 {
        if self.family_unix {
            return 1; // AF_UNIX
        }
        match self.bound {
            Some(SockAddr::V4 { .. }) => 2,    // AF_INET
            Some(SockAddr::V6 { .. }) => 10,   // AF_INET6
            Some(SockAddr::Unix { .. }) => 1,  // AF_UNIX (defensive — family_unix catches first)
            None => 2,                          // default AF_INET for unbound
        }
    }
}

/// P2-C3 part 2: AF_UNIX socket state.
///
/// A single struct carries all the AF_UNIX host-resource variants an
/// `AF_UNIX` fd might need over its lifetime:
/// * `listener` — set on `bind` + `listen` (SOCK_STREAM only).
/// * `stream`   — set on `connect` (SOCK_STREAM) or after `accept4`.
/// * `dgram`    — set on `socket(AF_UNIX, SOCK_DGRAM)` (lazy on first
///   `sendto` / `recvfrom`).
///
/// `path` is the filesystem path this socket is bound to (if any).
/// Used for `getsockname` write-back and for the close-time `unlink`.
/// Lock-discipline: same as everywhere else — never hold a
/// `parking_lot::Mutex` guard across `.await`.
#[allow(dead_code)]
pub struct UnixSockInner {
    pub path: Option<PathBuf>,
    pub listener: Option<UnixListener>,
    pub stream: Option<UnixStream>,
    pub dgram: Option<UnixDatagram>,
    /// Peer address — for AF_UNIX this is a `std::os::unix::net::SocketAddr`
    /// (path-based). Used by `getpeername` write-back.
    pub peer_addr: Option<std::os::unix::net::SocketAddr>,
}

impl UnixSockInner {
    pub fn new() -> Self {
        Self {
            path: None,
            listener: None,
            stream: None,
            dgram: None,
            peer_addr: None,
        }
    }
}

/// P1-7: per-fd entry in an `Epoll` instance. Stores the requested events
/// mask, the user-supplied data word, and a `Notify` the epoll_wait async
/// future subscribes to.
#[allow(dead_code)]
#[derive(Clone)]
pub struct EpollEntry {
    pub fd: u32,
    pub events: u32,
    pub data: u64,
    /// Wake primitive shared with the watched fd's `Notify`. The
    /// `epoll_wait` task awaits on this; the fd's writers call
    /// `notify.notify_waiters()` to wake it.
    pub wake: Arc<tokio::sync::Notify>,
}

/// P1-7: kernel-side state for an `epoll_create1` fd.
#[allow(dead_code)]
pub struct EpollInner {
    /// Active registrations, keyed by the watched fd.
    pub entries: parking_lot::Mutex<std::collections::HashMap<u32, EpollEntry>>,
    /// Wake primitive used to cancel a pending `epoll_wait`. When
    /// `epoll_ctl(DEL)` runs while a wait is in flight, it notifies here
    /// so the wait re-scans and returns 0 events.
    pub cancel: Arc<tokio::sync::Notify>,
    /// `eventfd` auto-created and registered with EPOLLIN as a self-wake
    /// primitive. P1-7's epoll_wait awaits on this plus the per-fd wakes.
    pub self_event_fd: Option<u32>,
}

/// P1-7: kernel-side state for an `eventfd2` fd.
#[allow(dead_code)]
pub struct EventFdInner {
    pub counter: parking_lot::Mutex<u64>,
    pub notify: Arc<tokio::sync::Notify>,
    pub nonblock: AtomicBool,
}

/// A `Resource` is what's behind a fd. Variants fill in as syscalls land.
///
/// P2-B5: `File` and `Socket` are wrapped in `Arc<Mutex<...>>` so dup'd
/// fds share an open-file description. `Stdin`/`Stdout`/`Stderr`/
/// `PipeRead`/`PipeWrite` were already sharing inner state via per-field
/// `Arc<Mutex<VecDeque<u8>>>` etc. (see `make_pipe()` above); only the
/// outer enum variant needs no change for those. `Epoll` and `EventFd`
/// continue to reject `dup()` with -EBADF.
pub enum Resource {
    /// stdin — typically a `PipeRead` preloaded by the driver.
    Stdin(PipeRead),
    /// stdout — typically a `PipeWrite` preloaded by the driver.
    Stdout(PipeWrite),
    /// stderr — typically a `PipeWrite` preloaded by the driver.
    Stderr(PipeWrite),
    /// Opened file (Step 14). P2-B5: shared via `Arc<Mutex<>>` so dup'd
    /// fds share the open-file description (offset, path, dir cache).
    File(SharedFilePos),
    /// Read end of a `pipe2` (Step 15).
    #[allow(dead_code)]
    PipeRead(PipeRead),
    /// Write end of a `pipe2` (Step 15).
    #[allow(dead_code)]
    PipeWrite(PipeWrite),
    /// Socket fd created by `socket(2)` (P1-1). P2-B5: shared via
    /// `Arc<Mutex<>>` so `dup()`-style fds share `bound`/`listener`/etc.
    #[allow(dead_code)]
    Socket(SharedSocket),
    /// Epoll instance created by `epoll_create1(2)` (P1-7).
    #[allow(dead_code)]
    Epoll(EpollInner),
    /// Event counter created by `eventfd2(2)` (P1-7).
    #[allow(dead_code)]
    EventFd(EventFdInner),
}

/// P2-B5: shared-state wrappers for dup-able resource variants. Both
/// use `parking_lot::Mutex` (sync; **never hold across `.await`**).
///
/// Locking rule (every site in `sys/socket.rs`, `sys/file.rs`,
/// `sys/poll.rs`, `sys/epoll.rs` follows this):
///   1. Lock briefly to read/copy state out (or `Option::take()` it).
///   2. Drop the guard before any `.await`.
///   3. Re-acquire the lock briefly to write the result back.
pub type SharedFilePos = std::sync::Arc<parking_lot::Mutex<crate::sys::file::FilePos>>;
pub type SharedSocket = std::sync::Arc<parking_lot::Mutex<SocketInner>>;

pub struct FdTable {
    table: HashMap<u32, Resource>,
    next_fd: u32,
    /// P2-B5: set of fds whose FD_CLOEXEC bit is set. `F_DUPFD_CLOEXEC`,
    /// `dup3` with `O_CLOEXEC`, and `F_SETFD(1)` insert; `close()` removes.
    cloexec: std::collections::HashSet<u32>,
}

impl FdTable {
    /// Construct an empty table (no stdio preloaded). Tests use this.
    pub fn empty() -> Self {
        Self {
            table: HashMap::new(),
            next_fd: 3,
            cloexec: std::collections::HashSet::new(),
        }
    }

    /// Test-only: insert a resource at a specific fd.
    #[allow(dead_code)]
    pub fn table_mut_for_test(&mut self) -> &mut HashMap<u32, Resource> {
        &mut self.table
    }

    /// Construct with stdin/stdout/stderr backed by buffer pipes. Tests
    /// use this so they can inspect what the guest wrote.
    pub fn with_buffered_stdio() -> Self {
        let (rd, wr_out) = make_pipe();
        let (_rd_err, wr_err) = make_pipe();
        let mut table = HashMap::new();
        table.insert(STDIN, Resource::Stdin(rd));
        table.insert(STDOUT, Resource::Stdout(wr_out));
        table.insert(STDERR, Resource::Stderr(wr_err));
        Self {
            table,
            next_fd: 3,
            cloexec: std::collections::HashSet::new(),
        }
    }

    /// Insert a resource and return its fd.
    pub fn insert(&mut self, r: Resource) -> u32 {
        let fd = self.next_fd;
        self.next_fd += 1;
        self.table.insert(fd, r);
        fd
    }

    /// P2-B5: insert at a specific fd. Returns `Err(-EBADF)` if the
    /// fd is already bound. Caller is responsible for pre-closing.
    pub fn insert_at(&mut self, fd: u32, r: Resource) -> Result<u32, i64> {
        if self.table.contains_key(&fd) {
            return Err(-(crate::errno::EBADF));
        }
        if fd >= self.next_fd {
            self.next_fd = fd + 1;
        }
        self.table.insert(fd, r);
        Ok(fd)
    }

    /// P2-B5: insert at the lowest free fd ≥ `min`. Used by
    /// `fcntl(F_DUPFD, min_fd)` so the returned fd honors the
    /// minimum-fd argument per Linux semantics: the kernel does not
    /// pre-fill slots below `min` — if `min` is free it IS the answer,
    /// even when `next_fd` is higher. We bump `next_fd` only if the
    /// chosen fd exceeds it.
    pub fn insert_at_least(&mut self, min: u32, r: Resource) -> u32 {
        let mut fd = min;
        while self.table.contains_key(&fd) {
            fd = fd.saturating_add(1);
        }
        if fd >= self.next_fd {
            self.next_fd = fd + 1;
        }
        self.table.insert(fd, r);
        fd
    }

    /// P2-B5: set the FD_CLOEXEC bit for `fd`.
    pub fn set_cloexec(&mut self, fd: u32, on: bool) {
        if on {
            self.cloexec.insert(fd);
        } else {
            self.cloexec.remove(&fd);
        }
    }

    /// P2-B5: returns true if `fd`'s FD_CLOEXEC bit is set.
    pub fn get_cloexec(&self, fd: u32) -> bool {
        self.cloexec.contains(&fd)
    }

    /// Borrow the resource behind `fd`. Returns `Err(-EBADF)` for unknown.
    pub fn get(&self, fd: u32) -> Result<&Resource, i64> {
        self.table.get(&fd).ok_or(-(crate::errno::EBADF))
    }

    /// Mutably borrow the resource behind `fd`.
    pub fn get_mut(&mut self, fd: u32) -> Result<&mut Resource, i64> {
        self.table.get_mut(&fd).ok_or(-(crate::errno::EBADF))
    }

    /// Close `fd`, returning `Err(-EBADF)` if it doesn't exist. P2-B5:
    /// also removes the fd from `cloexec` so the set stays consistent.
    pub fn close(&mut self, fd: u32) -> Result<(), i64> {
        if self.table.remove(&fd).is_some() {
            self.cloexec.remove(&fd);
            Ok(())
        } else {
            Err(-(crate::errno::EBADF))
        }
    }

    /// True if `fd` is currently bound.
    pub fn contains(&self, fd: u32) -> bool {
        self.table.contains_key(&fd)
    }
}

impl Default for FdTable {
    fn default() -> Self {
        Self::with_buffered_stdio()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_has_no_fds() {
        let t = FdTable::empty();
        assert!(!t.contains(STDIN));
        assert!(!t.contains(STDOUT));
    }

    #[test]
    fn buffered_stdio_preloads_012() {
        let t = FdTable::with_buffered_stdio();
        assert!(t.contains(STDIN));
        assert!(t.contains(STDOUT));
        assert!(t.contains(STDERR));
    }

    #[test]
    fn insert_increments_next_fd() {
        let mut t = FdTable::empty();
        let (rd, wr) = make_pipe();
        let a = t.insert(Resource::PipeRead(rd));
        let b = t.insert(Resource::PipeWrite(wr));
        assert_eq!(a, 3);
        assert_eq!(b, 4);
    }

    #[test]
    fn close_removes_fd() {
        let mut t = FdTable::empty();
        let (rd, _) = make_pipe();
        let fd = t.insert(Resource::PipeRead(rd));
        assert!(t.close(fd).is_ok());
        assert!(!t.contains(fd));
        assert_eq!(t.close(fd), Err(-crate::errno::EBADF));
    }

    #[test]
    fn pipe_roundtrip() {
        let (mut rd, mut wr) = make_pipe();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            wr.write_all(b"hello").await.unwrap();
            wr.shutdown().await.unwrap();
            let mut out = Vec::new();
            rd.read_to_end(&mut out).await.unwrap();
            assert_eq!(out, b"hello");
        });
    }
}
