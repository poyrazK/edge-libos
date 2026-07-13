//! Per-process file-descriptor table.
//!
//! P0 covers stdin / stdout / stderr only. Each stdio fd is backed by a
//! `BufPipe`-style buffer that lives in the kernel; tests can construct a
//! Kernel with their own buffers, the binary driver uses the host stdio.
//!
//! The `Resource` enum fills in as VFS lands (File, Dir, PipeRead/Write,
//! eventually Socket/Epoll in P1).

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicI32};
use std::sync::Arc;

use parking_lot::Mutex;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::{TcpListener, TcpStream};

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
    (
        PipeRead {
            buf: buf.clone(),
            closed: closed.clone(),
            nonblock: nonblock.clone(),
        },
        PipeWrite {
            buf,
            closed,
            nonblock,
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
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SockAddr {
    V4 { port: u16, addr: [u8; 4] },
    V6 { port: u16, addr: [u8; 16] },
}

impl SockAddr {
    /// Build a `SocketAddrV4` from a `SockAddr::V4`. Returns `None` for V6
    /// (handled separately when we add IPv6 listener support).
    pub fn as_v4(&self) -> Option<std::net::SocketAddrV4> {
        match self {
            SockAddr::V4 { port, addr } => {
                std::net::SocketAddrV4::new(std::net::Ipv4Addr::from(*addr), *port).into()
            }
            SockAddr::V6 { .. } => None,
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
        }
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

    /// P1-6: family inferred from `bound` (or AF_INET if unknown).
    pub fn family(&self) -> i32 {
        match self.bound {
            Some(SockAddr::V4 { .. }) => 2,    // AF_INET
            Some(SockAddr::V6 { .. }) => 10,   // AF_INET6
            None => 2,                          // default AF_INET for unbound
        }
    }
}

/// A `Resource` is what's behind a fd. Variants fill in as syscalls land.
pub enum Resource {
    /// stdin — typically a `PipeRead` preloaded by the driver.
    Stdin(PipeRead),
    /// stdout — typically a `PipeWrite` preloaded by the driver.
    Stdout(PipeWrite),
    /// stderr — typically a `PipeWrite` preloaded by the driver.
    Stderr(PipeWrite),
    /// Opened file (Step 14).
    File(crate::sys::file::FilePos),
    /// Read end of a `pipe2` (Step 15).
    #[allow(dead_code)]
    PipeRead(PipeRead),
    /// Write end of a `pipe2` (Step 15).
    #[allow(dead_code)]
    PipeWrite(PipeWrite),
    /// Socket fd created by `socket(2)` (P1-1).
    #[allow(dead_code)]
    Socket(SocketInner),
}

pub struct FdTable {
    table: HashMap<u32, Resource>,
    next_fd: u32,
}

impl FdTable {
    /// Construct an empty table (no stdio preloaded). Tests use this.
    pub fn empty() -> Self {
        Self {
            table: HashMap::new(),
            next_fd: 3,
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
        }
    }

    /// Insert a resource and return its fd.
    pub fn insert(&mut self, r: Resource) -> u32 {
        let fd = self.next_fd;
        self.next_fd += 1;
        self.table.insert(fd, r);
        fd
    }

    /// Borrow the resource behind `fd`. Returns `Err(-EBADF)` for unknown.
    pub fn get(&self, fd: u32) -> Result<&Resource, i64> {
        self.table.get(&fd).ok_or(-(crate::errno::EBADF))
    }

    /// Mutably borrow the resource behind `fd`.
    pub fn get_mut(&mut self, fd: u32) -> Result<&mut Resource, i64> {
        self.table.get_mut(&fd).ok_or(-(crate::errno::EBADF))
    }

    /// Close `fd`, returning `Err(-EBADF)` if it doesn't exist.
    pub fn close(&mut self, fd: u32) -> Result<(), i64> {
        if self.table.remove(&fd).is_some() {
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
