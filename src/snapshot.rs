//! P2-D1 — Snapshot foundation.
//!
//! A `KernelSnapshot` is a serializable copy of the per-store `Kernel`
//! state, suitable for replay with `postcard` round-tripping. It is NOT
//! a verbatim clone of `Kernel` — fields holding OS handles (`std::fs::File`,
//! `tokio::net::TcpListener`, `Arc<tokio::sync::Notify>`, `Memory`) are
//! dropped and rebuilt on `apply_snapshot`.
//!
//! ## What is persisted
//!
//! Every field on `KernelSnapshot` is something we can either write as
//! bytes or reopen from data on restore. The fd table is flattened into
//! `Vec<FdEntrySnapshot>` so postcard encoding is deterministic across
//! runs (no `HashMap` insertion order dependence).
//!
//! ## What is dropped
//!
//! - `wasmtime::Memory` — re-attached via `Kernel::attach_memory`.
//! - `Arc<tokio::sync::Notify>` — pending waiters are lost; the guest
//!   re-registers on its next syscall.
//! - `parking_lot::Mutex<…>` lock guards — locked briefly, inner snapshotted.
//! - `Kernel.started_at: Instant` — re-anchored on restore; monotonic
//!   clock recomputed against `Instant::now()`.
//! - `SmallRng`'s CHACHA state — replaced by `rng_seed: [u8; 32]` captured
//!   at construction; rebuilt via `SmallRng::from_seed`.
//! - `std::fs::File`, `TcpListener`, `TcpStream`, `UnixListener`,
//!   `UnixStream`, `UnixDatagram` — never serialized; the table below
//!   describes how each is reopened.
//!
//! ## Restore strategy (handled by `apply_snapshot`)
//!
//! | Runtime handle | Snapshot fields used | Reopen API |
//! |---|---|---|
//! | `Resource::File` | `FileSnapshot { path, pos, is_dir, dir_cache }` | `OpenOptions::open(path)` + `seek(Start(pos))` |
//! | `Resource::Socket` listener (IPv4/V6) | `bound`, `so_reuseaddr` | `TcpListener::bind(addr)` (+ `SO_REUSEADDR` if set) |
//! | `Resource::Socket` accepted stream | n/a | `SnapshotError::Unsupported("accepted stream on listener")` — D3 decides how to handle |
//! | `Resource::Socket` Unix listener (filesystem-path) | `unix_inner.path` | `UnixListener::bind(path)` |
//! | `Resource::Socket` Unix listener (abstract) | n/a | `SnapshotError::Unsupported("abstract unix namespace")` |
//! | `Resource::Socket` Unix stream / datagram | n/a | `SnapshotError::Unsupported("accepted unix stream")` for now |
//! | `Resource::Epoll` | `EpollSnapshot { entries, self_event_fd }` | rebuild via `epoll_create1`/`epoll_ctl` |
//! | `Resource::EventFd` | `EventFdSnapshot { counter, nonblock }` | rebuild via `eventfd2` + write `counter` bytes |
//! | `Resource::Pipe*` / stdio | `PipeSnapshot { buf, closed, nonblock }` | `make_pipe` + buffer replay |
//!
//! ## Format versioning
//!
//! Every snapshot starts with `format_version: u32 = SNAPSHOT_FORMAT_VERSION`.
//! Future D-series changes bump the version and migrate on decode.

use std::path::PathBuf;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::fd::SockAddr;
use crate::kernel::{Kernel, RngSeed};
use crate::sys::signal::SignalState;
use crate::vfs::Vfs;

pub const SNAPSHOT_FORMAT_VERSION: u32 = 1;

/// All snapshot types live in this module. They are independent of the
/// runtime `Kernel` so the snapshot shape can evolve without disturbing
/// live handler code.

/// An adapter that maps `VecDeque<u8>` ↔ `Vec<u8>` for serde.
///
/// `std::collections::VecDeque` does not derive `Serialize`, but it does
/// implement `From<Vec<T>>` and `Into<Vec<T>>`. We piggy-back on the
/// `Vec<u8>` serde impls.
pub mod vecdeque_bytes {
    use std::collections::VecDeque;

    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<S: Serializer>(dq: &VecDeque<u8>, s: S) -> Result<S::Ok, S::Error> {
        // Build a `Vec<u8>` from the deque's contents. Cheaper than
        // allocating a fresh Vec; we cannot borrow the deque's storage
        // directly into postcard, so we copy.
        let v: Vec<u8> = dq.iter().copied().collect();
        v.serialize(s)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<VecDeque<u8>, D::Error> {
        let v: Vec<u8> = Vec::<u8>::deserialize(d)?;
        Ok(VecDeque::from(v))
    }
}

/// Helper trait for the `Arc<parking_lot::Mutex<T>>` pattern used in fd.rs.
///
/// P2-D1: this module is not yet used by the snapshot types — it is
/// retained as a hook for D2 (when `Resource::File` lands in scope).
/// Once a Resource holds an `Arc<parking_lot::Mutex<FilePos>>` and the
/// snapshot needs to drain it, this module plugs in directly.
#[allow(dead_code)]
pub mod parking_lot_mutex_bytes {
    use parking_lot::Mutex;
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<T, S>(m: &Mutex<T>, s: S) -> Result<S::Ok, S::Error>
    where
        T: Serialize,
        S: Serializer,
    {
        // Lock briefly, serialize the inner, drop the guard. Per project
        // rule: never hold a `parking_lot::Mutex` guard across `.await`,
        // and serialization may internally `.await` (via postcard IO
        // patterns), so this MUST not be a deadlock risk.
        let guard = m.lock();
        let r = (*guard).serialize(s);
        drop(guard);
        r
    }

    pub fn deserialize<'de, T, D>(d: D) -> Result<Mutex<T>, D::Error>
    where
        T: Deserialize<'de>,
        D: Deserializer<'de>,
    {
        let inner: T = T::deserialize(d)?;
        Ok(Mutex::new(inner))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KernelSnapshot {
    pub format_version: u32,
    pub fds: FdSnapshot,
    pub mm: LinearAllocatorSnapshot,
    pub vfs: VfsSnapshot,
    pub clock: ClockStateSnapshot,
    pub brk: u32,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    pub rng_seed: RngSeed,
    pub signals: SignalStateSnapshot,
    pub exit_code: Option<i32>,
    pub comm: [u8; 16],
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct FdSnapshot {
    /// Sorted by `(fd,)` for deterministic postcard output.
    pub entries: Vec<FdEntrySnapshot>,
    pub next_fd: u32,
    /// Sorted ascending.
    pub cloexec: Vec<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FdEntrySnapshot {
    pub fd: u32,
    pub kind: ResourceSnapshot,
}

/// P2-D1: per-variant kind tag is a string and the payload is a
/// single struct field. We use an explicit field rather than an
/// internally-tagged enum because `postcard` (1.x) does not support
/// internally-tagged enums out of the box — only externally tagged
/// (with variant index) or adjacent/enum-with-content. The single-
/// field-with-tag form below serializes as `{ "kind": "stdin", "body":
/// PipeSnapshot { ... } }`. Use `bincode`-style adjacent with explicit
/// struct field; deserializes reliably across postcard versions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResourceSnapshot {
    pub kind: ResourceKind,
    pub body: ResourceBody,
}

/// All `Resource` variants enumerated as a serde-friendly enum.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum ResourceKind {
    Stdin,
    Stdout,
    Stderr,
    PipeRead,
    PipeWrite,
    File,
    Socket,
    Epoll,
    EventFd,
}

/// The per-kind payload — flattened union.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ResourceBody {
    /// Pipe variants: serializes pipe state.
    pub pipe: Option<PipeSnapshot>,
    /// File variant: serializes file state.
    pub file: Option<FileSnapshot>,
    /// Socket variant: serializes socket state.
    pub socket: Option<SocketSnapshot>,
    /// Epoll variant: serializes epoll state.
    pub epoll: Option<EpollSnapshot>,
    /// EventFd variant: serializes eventfd state.
    pub eventfd: Option<EventFdSnapshot>,
}

impl ResourceSnapshot {
    /// Build from a runtime `Resource` kind and the relevant snapshot
    /// form. Returns a typed value; the caller chooses the
    /// corresponding discriminator.
    pub fn from_pipe(kind: ResourceKind, pipe: PipeSnapshot) -> Self {
        debug_assert!(matches!(kind, ResourceKind::Stdin | ResourceKind::Stdout | ResourceKind::Stderr | ResourceKind::PipeRead | ResourceKind::PipeWrite));
        let mut body = ResourceBody::default();
        body.pipe = Some(pipe);
        Self { kind, body }
    }

    pub fn from_file(file: FileSnapshot) -> Self {
        let mut body = ResourceBody::default();
        body.file = Some(file);
        Self { kind: ResourceKind::File, body }
    }

    pub fn from_socket(socket: SocketSnapshot) -> Self {
        let mut body = ResourceBody::default();
        body.socket = Some(socket);
        Self { kind: ResourceKind::Socket, body }
    }

    pub fn from_epoll(epoll: EpollSnapshot) -> Self {
        let mut body = ResourceBody::default();
        body.epoll = Some(epoll);
        Self { kind: ResourceKind::Epoll, body }
    }

    pub fn from_eventfd(eventfd: EventFdSnapshot) -> Self {
        let mut body = ResourceBody::default();
        body.eventfd = Some(eventfd);
        Self { kind: ResourceKind::EventFd, body }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PipeSnapshot {
    #[serde(with = "vecdeque_bytes")]
    pub buf: std::collections::VecDeque<u8>,
    pub closed: bool,
    pub nonblock: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct FileSnapshot {
    pub path: Option<PathBuf>,
    pub pos: u64,
    pub is_dir: bool,
    pub dir_cache: Option<Vec<u8>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SocketSnapshot {
    pub sock_kind: crate::fd::SocketKind,
    pub nonblock: bool,
    pub bound: Option<SockAddr>,
    /// Recorded for fidelity; the OS picks the actual backlog on restore.
    pub listen_backlog: Option<i32>,
    pub so_reuseaddr: bool,
    pub so_keepalive: bool,
    pub tcp_nodelay: bool,
    pub peer_addr_present: bool,
    pub last_error: i32,
    pub shutdown_flags: u8,
    pub is_acceptor: bool,
    #[serde(with = "vecdeque_bytes")]
    pub peek_buf: std::collections::VecDeque<u8>,
    pub family_unix: bool,
    pub unix_inner: Option<UnixSockSnapshot>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct UnixSockSnapshot {
    pub path: Option<PathBuf>,
    /// P2-D1: cannot persist `std::os::unix::net::SocketAddr` (no
    /// `Serialize` on stable; `as_bytes`/`from_bytes` are unstable).
    /// The peer addr for AF_UNIX is filesystem-path-based and
    /// reconstructable from `path` on restore.
    pub peer_addr_present: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct EpollSnapshot {
    /// Vec (sorted) because serde on HashMap is non-deterministic.
    pub entries: Vec<EpollEntrySnapshot>,
    pub self_event_fd: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EpollEntrySnapshot {
    pub fd: u32,
    pub events: u32,
    pub data: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct EventFdSnapshot {
    pub counter: u64,
    pub nonblock: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct LinearAllocatorSnapshot {
    /// Identical shape to `crate::mm::Arena`; we serialize the runtime
    /// type directly because `Arena` already derives the right traits.
    pub arenas: Vec<crate::mm::Arena>,
    pub high_water: u32,
}

/// Identical-shape mirror of `crate::kernel::ClockState` so the runtime
/// type doesn't need a serde dep transitively. `apply_snapshot` writes
/// from this into the kernel's `ClockState`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ClockStateSnapshot {
    pub boot_monotonic_ns: u64,
}

/// Identical-shape mirror of `crate::sys::signal::SignalState`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SignalStateSnapshot {
    pub actions: std::collections::BTreeMap<i32, crate::sys::signal::SigAction>,
    pub mask: u64,
    pub alt_stack: Option<Vec<u8>>,
}

/// Identical-shape mirror of `crate::vfs::Vfs`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VfsSnapshot {
    pub root: PathBuf,
    pub cwd: PathBuf,
}

impl From<&Vfs> for VfsSnapshot {
    fn from(v: &Vfs) -> Self {
        Self {
            root: v.root.clone(),
            cwd: v.cwd.clone(),
        }
    }
}

impl From<&SignalState> for SignalStateSnapshot {
    fn from(s: &SignalState) -> Self {
        // Sort the actions map by signum for deterministic encoding.
        let mut actions: std::collections::BTreeMap<i32, crate::sys::signal::SigAction> =
            std::collections::BTreeMap::new();
        for (k, v) in &s.actions {
            actions.insert(*k, *v);
        }
        Self {
            actions,
            mask: s.mask,
            alt_stack: s.alt_stack.clone(),
        }
    }
}

impl From<&crate::kernel::ClockState> for ClockStateSnapshot {
    fn from(c: &crate::kernel::ClockState) -> Self {
        Self {
            boot_monotonic_ns: c.boot_monotonic_ns,
        }
    }
}

#[derive(Debug)]
pub enum SnapshotError {
    /// Snapshot format version mismatch. D-series bumps the version
    /// when the schema changes incompatibly.
    FormatVersionMismatch { found: u32, supported: u32 },
    /// An underlying `std::fs` call failed during snapshot or restore.
    IoError(std::io::Error, String),
    /// A snapshot referenced a path that no longer exists on restore.
    MissingPath(String),
    /// Snapshotted a state we explicitly do not support rebuilding yet
    /// (per the table above). D3 (the freeze CLI) may abort with this.
    Unsupported(&'static str),
    /// An already-accepted socket — see supported table.
    AcceptedStreamOnListener,
    /// Abstract unix namespace — not yet supported.
    AbstractUnixNamespace,
    /// Unknown resource variant encountered during decode.
    UnknownResource,
    Postcard(String),
}

impl std::fmt::Display for SnapshotError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SnapshotError::FormatVersionMismatch { found, supported } => write!(
                f,
                "snapshot format_version={found} does not match supported {supported}"
            ),
            SnapshotError::IoError(e, ctx) => {
                write!(f, "io error during snapshot ({ctx}): {e}")
            }
            SnapshotError::MissingPath(p) => write!(f, "missing path on restore: {p}"),
            SnapshotError::Unsupported(s) => write!(f, "unsupported snapshot case: {s}"),
            SnapshotError::AcceptedStreamOnListener => {
                write!(f, "accepted stream on listener")
            }
            SnapshotError::AbstractUnixNamespace => write!(f, "abstract unix namespace"),
            SnapshotError::UnknownResource => write!(f, "unknown resource variant"),
            SnapshotError::Postcard(s) => write!(f, "postcard error: {s}"),
        }
    }
}

impl std::error::Error for SnapshotError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            SnapshotError::IoError(e, _) => Some(e),
            _ => None,
        }
    }
}

/// Walk `Kernel` and assemble a `KernelSnapshot`.
///
/// Locks briefly per resource, drops the guard, copies out the snapshot
/// form. Runtime handles (`Arc<Notify>`, `Memory`, raw fds) are dropped
/// from the snapshot — they will be rebuilt in `apply_snapshot` (D1.7)
/// and the linear-memory in `apply_snapshot` (D2).
pub fn try_to_snapshot(
    kernel: &Kernel,
    _mem_bytes: &[u8],
) -> Result<KernelSnapshot, SnapshotError> {
    use crate::fd::Resource;
    use crate::snapshot::{FdEntrySnapshot, ResourceSnapshot};

    let mut entries: Vec<FdEntrySnapshot> = Vec::new();
    // Snapshot the fd table in sorted order for deterministic postcard output.
    let mut fds_sorted: Vec<(u32, &Resource)> = kernel.fds.iter_for_snapshot();
    fds_sorted.sort_by_key(|(fd, _)| *fd);

    for (fd, resource) in fds_sorted {
        let kind = match resource {
            Resource::Stdin(p) => ResourceSnapshot::from_pipe(ResourceKind::Stdin, p.snapshot()),
            Resource::Stdout(p) => ResourceSnapshot::from_pipe(ResourceKind::Stdout, p.snapshot()),
            Resource::Stderr(p) => ResourceSnapshot::from_pipe(ResourceKind::Stderr, p.snapshot()),
            Resource::PipeRead(p) => ResourceSnapshot::from_pipe(ResourceKind::PipeRead, p.snapshot()),
            Resource::PipeWrite(p) => ResourceSnapshot::from_pipe(ResourceKind::PipeWrite, p.snapshot()),
            Resource::File(shared) => {
                let guard = shared.lock();
                ResourceSnapshot::from_file(crate::snapshot::FileSnapshot {
                    path: guard.path.clone(),
                    pos: guard.pos,
                    is_dir: guard.is_dir,
                    dir_cache: guard.dir_cache.clone(),
                })
            }
            Resource::Socket(shared) => {
                let guard = shared.lock();
                ResourceSnapshot::from_socket(guard.snapshot())
            }
            Resource::Epoll(e) => ResourceSnapshot::from_epoll(e.snapshot()),
            Resource::EventFd(e) => ResourceSnapshot::from_eventfd(e.snapshot()),
        };
        entries.push(FdEntrySnapshot { fd, kind });
    }

    let cloexec: Vec<u32> = {
        let mut v: Vec<u32> = kernel.fds.iter_cloexec_for_snapshot();
        v.sort();
        v
    };

    Ok(KernelSnapshot {
        format_version: SNAPSHOT_FORMAT_VERSION,
        fds: crate::snapshot::FdSnapshot {
            entries,
            next_fd: kernel.fds.next_fd_for_snapshot(),
            cloexec,
        },
        mm: kernel.mm.snapshot(),
        vfs: crate::snapshot::VfsSnapshot::from(&kernel.vfs),
        clock: crate::snapshot::ClockStateSnapshot::from(&kernel.clock),
        brk: kernel.brk,
        args: kernel.args.clone(),
        env: kernel.env.clone(),
        rng_seed: kernel.rng_seed,
        signals: crate::snapshot::SignalStateSnapshot::from(&kernel.signals),
        exit_code: kernel.exit_code,
        comm: kernel.comm,
    })
}

/// Placeholder — D1.7 will fill this in.
pub fn apply_snapshot(
    _snap: KernelSnapshot,
    _kernel: &mut Kernel,
    _mem: &mut Vec<u8>,
) -> Result<(), SnapshotError> {
    Err(SnapshotError::Postcard(
        "apply_snapshot: not yet implemented (D1.7)".into(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::SeedableRng;
    use std::sync::Arc;

    #[test]
    fn format_version_constant_value() {
        assert_eq!(SNAPSHOT_FORMAT_VERSION, 1);
    }

    #[test]
    fn smoke_postcard_roundtrip_of_format_version_only() {
        // Encode a minimal snapshot via postcard and decode it back.
        let snap = KernelSnapshot {
            format_version: SNAPSHOT_FORMAT_VERSION,
            fds: FdSnapshot::default(),
            mm: LinearAllocatorSnapshot::default(),
            vfs: VfsSnapshot {
                root: "/".into(),
                cwd: "/".into(),
            },
            clock: ClockStateSnapshot::default(),
            brk: 0,
            args: vec!["a".to_string()],
            env: vec![("K".to_string(), "V".to_string())],
            rng_seed: [7u8; 32],
            signals: SignalStateSnapshot::default(),
            exit_code: None,
            comm: [0u8; 16],
        };
        let bytes = postcard::to_stdvec(&snap).expect("encode");
        let back: KernelSnapshot = postcard::from_bytes(&bytes).expect("decode");
        // Field-by-field; KernelSnapshot doesn't derive PartialEq.
        assert_eq!(back.format_version, 1);
    }

    #[test]
    fn vecdeque_adapter_roundtrips() {
        let mut dq = std::collections::VecDeque::new();
        for b in 0..16u8 {
            dq.push_back(b);
        }
        let snap = PipeSnapshot {
            buf: dq.clone(),
            closed: false,
            nonblock: true,
        };
        let bytes = postcard::to_stdvec(&snap).expect("encode");
        let back: PipeSnapshot = postcard::from_bytes(&bytes).expect("decode");
        assert_eq!(back.buf, dq);
        assert_eq!(back.closed, false);
        assert_eq!(back.nonblock, true);
    }

    #[test]
    fn linear_allocator_snapshot_roundtrip() {
        // Build a snapshot directly and round-trip.
        let lsnap = LinearAllocatorSnapshot {
            arenas: vec![crate::mm::Arena::new(0x1_000_0000)],
            high_water: 0x1_001_0000,
        };
        let bytes = postcard::to_stdvec(&lsnap).expect("encode");
        let back: LinearAllocatorSnapshot = postcard::from_bytes(&bytes).expect("decode");
        // Field-by-field checks; PartialEq was dropped to avoid Eq-bound
        // chains through SocketAddr-free paths.
        assert_eq!(back.high_water, 0x1_001_0000);
        assert_eq!(back.arenas.len(), 1);
        assert_eq!(back.arenas[0].base, 0x1_000_0000);
        assert_eq!(back.arenas[0].used, 0);
        assert!(back.arenas[0].free_list.is_empty());
    }

    #[test]
    fn sanity_snapshot_roundtrip() {
        // Plan §Verification: build a real Kernel (with stdio + an
        // EventFd), snap it, encode/decode via postcard, verify fields.
        use crate::fd::{EventFdInner, Resource};
        use std::sync::atomic::AtomicBool;

        let kernel = Kernel::new_without_stdio(
            vec!["edge-python".into(), "main.py".into()],
            vec![("PATH".to_string(), "/usr/bin".to_string())],
        );

        // Force a specific RNG seed so we can compare.
        // (Default uses OS entropy.)
        let mut kernel = kernel;
        kernel.rng_seed = [42u8; 32];
        kernel.rng = rand::rngs::SmallRng::from_seed(kernel.rng_seed);
        kernel.brk = 0x1000;
        kernel.comm[0] = b'e';
        kernel.comm[1] = b'd';
        kernel.comm[2] = b'g';
        kernel.comm[3] = b'e';

        // Insert an EventFd at fd 3 (the first non-stdio slot).
        let efd_fd = kernel.fds.insert(Resource::EventFd(EventFdInner {
            counter: parking_lot::Mutex::new(7),
            notify: Arc::new(tokio::sync::Notify::new()),
            nonblock: AtomicBool::new(false),
        }));
        assert_eq!(efd_fd, 3);

        // Capture the snapshot.
        let snap = try_to_snapshot(&kernel, &[]).expect("snapshot succeeds");

        // Header.
        assert_eq!(snap.format_version, SNAPSHOT_FORMAT_VERSION);
        assert_eq!(snap.brk, 0x1000);
        assert_eq!(snap.rng_seed, [42u8; 32]);
        assert_eq!(snap.args, vec!["edge-python".to_string(), "main.py".to_string()]);
        assert_eq!(snap.env, vec![("PATH".to_string(), "/usr/bin".to_string())]);
        assert_eq!(snap.comm, kernel.comm);

        // FDs: 0 (stdin), 1 (stdout), 2 (stderr), 3 (eventfd) all present.
        assert_eq!(snap.fds.entries.len(), 4);
        let fds: Vec<u32> = snap.fds.entries.iter().map(|e| e.fd).collect();
        assert_eq!(fds, vec![0, 1, 2, 3]);

        // Specifically the EventFd entry has counter=7.
        let efd_entry = snap.fds.entries.iter().find(|e| e.fd == 3).unwrap();
        assert_eq!(efd_entry.kind.kind, ResourceKind::EventFd);
        let efd = efd_entry.kind.body.eventfd.as_ref().expect("eventfd body");
        assert_eq!(efd.counter, 7);
        assert!(!efd.nonblock);

        // next_fd should be ≥ 4.
        assert!(snap.fds.next_fd >= 4);

        // Round-trip the entire snapshot via postcard.
        let bytes = postcard::to_stdvec(&snap).expect("encode succeeds");
        let back: KernelSnapshot =
            postcard::from_bytes(&bytes).expect("decode succeeds");
        assert_eq!(back.format_version, snap.format_version);
        assert_eq!(back.brk, snap.brk);
        assert_eq!(back.rng_seed, snap.rng_seed);
        assert_eq!(back.args, snap.args);
        assert_eq!(back.env, snap.env);
        assert_eq!(back.fds.entries.len(), snap.fds.entries.len());
        assert_eq!(back.mm.high_water, snap.mm.high_water);

        // Round-trip the entire snapshot via postcard.
        let bytes = postcard::to_stdvec(&snap).expect("encode succeeds");
        let back: KernelSnapshot =
            postcard::from_bytes(&bytes).expect("decode succeeds");
        assert_eq!(back.format_version, snap.format_version);
        assert_eq!(back.brk, snap.brk);
        assert_eq!(back.rng_seed, snap.rng_seed);
        assert_eq!(back.args, snap.args);
        assert_eq!(back.env, snap.env);
        assert_eq!(back.fds.entries.len(), snap.fds.entries.len());
        assert_eq!(back.mm.high_water, snap.mm.high_water);
    }
}
