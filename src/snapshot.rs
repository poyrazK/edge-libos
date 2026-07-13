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

use std::net::SocketAddr;
use std::path::PathBuf;

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

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ResourceSnapshot {
    Stdin(PipeSnapshot),
    Stdout(PipeSnapshot),
    Stderr(PipeSnapshot),
    PipeRead(PipeSnapshot),
    PipeWrite(PipeSnapshot),
    File(FileSnapshot),
    Socket(SocketSnapshot),
    Epoll(EpollSnapshot),
    EventFd(EventFdSnapshot),
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
    pub peer_addr: Option<SocketAddr>,
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
    /// `std::os::unix::net::SocketAddr` is not `Serialize`/`Deserialize`
    /// on stable Rust. Persist as a presence tag — the peer addr for
    /// AF_UNIX is path-based and reconstructable from `path`.
    #[serde(default, with = "unix_socket_addr_drop")]
    pub peer_addr: Option<std::os::unix::net::SocketAddr>,
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
    pub arenas: Vec<ArenaSnapshot>,
    pub high_water: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArenaSnapshot {
    pub base: u32,
    pub used: usize,
    pub free_list: Vec<(usize, usize)>,
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

/// Adapter for `std::os::unix::net::SocketAddr`. The type isn't
/// `Serialize`/`Deserialize` on stable Rust and `as_bytes`/`from_bytes`
/// are unstable. We drop the unix peer addr on snapshot — peer info
/// for an AF_UNIX socket is filesystem-path-based and reconstructable
/// from `path` + `peer_path` on restore.
pub mod unix_socket_addr_drop {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use std::os::unix::net::SocketAddr;

    pub fn serialize<S: Serializer>(
        _addr: &Option<SocketAddr>,
        s: S,
    ) -> Result<S::Ok, S::Error> {
        // Persist as `()` so postcard has a fixed shape; on restore the
        // peer addr is reconstructed from path state, not from this field.
        ().serialize(s)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(_d: D) -> Result<Option<SocketAddr>, D::Error> {
        Ok(None)
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

/// Placeholder — the orchestration that walks `Kernel` and assembles the
/// snapshot lives in `snapshot_apply.rs`. D1.6 will fill this in.
pub fn try_to_snapshot(kernel: &Kernel, _mem_bytes: &[u8]) -> Result<KernelSnapshot, SnapshotError> {
    let _ = (kernel, _mem_bytes);
    Err(SnapshotError::Postcard(
        "try_to_snapshot: not yet implemented (D1.6)".into(),
    ))
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
            arenas: vec![ArenaSnapshot {
                base: 0x1_000_0000,
                used: 128,
                free_list: vec![(128, 64), (256, 32)],
            }],
            high_water: 0x1_001_0000,
        };
        let bytes = postcard::to_stdvec(&lsnap).expect("encode");
        let back: LinearAllocatorSnapshot = postcard::from_bytes(&bytes).expect("decode");
        // Field-by-field checks; PartialEq was dropped to avoid Eq-bound
        // chains through SocketAddr-free paths.
        assert_eq!(back.high_water, 0x1_001_0000);
        assert_eq!(back.arenas.len(), 1);
        assert_eq!(back.arenas[0].base, 0x1_000_0000);
        assert_eq!(back.arenas[0].used, 128);
        assert_eq!(back.arenas[0].free_list, vec![(128, 64), (256, 32)]);
    }
}
