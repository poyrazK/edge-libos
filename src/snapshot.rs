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

use crate::fd::FdTable;
use crate::fd::SockAddr;
use crate::kernel::{Kernel, RngSeed};
use crate::snapshot::endian::{LeI32, LeI64, LeU32, LeU64};
use crate::sys::signal::SignalState;
use crate::vfs::Vfs;

use rand::SeedableRng;

pub const SNAPSHOT_FORMAT_VERSION: u32 = 1;

/// ADR 0002 §3: snapshot linear memory in 64 KiB pages, sparse-encoded
/// (`Vec<MemoryPageSnapshot>`). Untouched (all-zero) pages are omitted.
pub const PAGE_SIZE_BYTES: usize = 64 * 1024;

/// P2-D2 / ADR 0002 §3: one 64 KiB page of linear memory. Missing from
/// the snapshot ⇒ that page is the wasmtime-grown zero-fill at restore
/// time. Wire order per page: `LeU32 page_index, LeU32 page_len, Vec<u8>
/// page_bytes`. We omit the redundant `page_len` (always
/// `PAGE_SIZE_BYTES` when present) — see the §3 example header for the
/// optional third field; we keep `Vec<u8>` always at PAGE_SIZE_BYTES.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryPageSnapshot {
    pub page_index: LeU32,
    pub bytes: Vec<u8>,
}

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

/// P2-D2 / ADR 0002 §2 — explicit little-endian newtypes for every
/// multi-byte integer in the snapshot wire format. See
/// `crate::snapshot::endian` for the rationale + wire-format spec.
pub mod endian;

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
    /// ADR 0002 §3 + §4: first wire word is `LeU32(1)` (or
    /// equivalent `u8` per §4 — LeU32 form subsumes it).
    pub format_version: LeU32,
    /// ADR 0002 §3: sparse per-page linear-memory overlay. Pages
    /// untouched by the guest (all-zero) are omitted from the snapshot
    /// and re-instated by wasmtime's grow on restore.
    pub pages: Vec<MemoryPageSnapshot>,
    pub fds: FdSnapshot,
    pub mm: LinearAllocatorSnapshot,
    pub vfs: VfsSnapshot,
    pub clock: ClockStateSnapshot,
    pub brk: LeU32,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    pub rng_seed: RngSeed,
    pub signals: SignalStateSnapshot,
    pub exit_code: Option<LeI32>,
    pub comm: [u8; 16],
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct FdSnapshot {
    /// Sorted by `(fd,)` for deterministic postcard output.
    pub entries: Vec<FdEntrySnapshot>,
    pub next_fd: LeU32,
    /// Sorted ascending.
    pub cloexec: Vec<LeU32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FdEntrySnapshot {
    pub fd: LeU32,
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
        debug_assert!(matches!(
            kind,
            ResourceKind::Stdin
                | ResourceKind::Stdout
                | ResourceKind::Stderr
                | ResourceKind::PipeRead
                | ResourceKind::PipeWrite
        ));
        let mut body = ResourceBody::default();
        body.pipe = Some(pipe);
        Self { kind, body }
    }

    pub fn from_file(file: FileSnapshot) -> Self {
        let mut body = ResourceBody::default();
        body.file = Some(file);
        Self {
            kind: ResourceKind::File,
            body,
        }
    }

    pub fn from_socket(socket: SocketSnapshot) -> Self {
        let mut body = ResourceBody::default();
        body.socket = Some(socket);
        Self {
            kind: ResourceKind::Socket,
            body,
        }
    }

    pub fn from_epoll(epoll: EpollSnapshot) -> Self {
        let mut body = ResourceBody::default();
        body.epoll = Some(epoll);
        Self {
            kind: ResourceKind::Epoll,
            body,
        }
    }

    pub fn from_eventfd(eventfd: EventFdSnapshot) -> Self {
        let mut body = ResourceBody::default();
        body.eventfd = Some(eventfd);
        Self {
            kind: ResourceKind::EventFd,
            body,
        }
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
    pub pos: LeU64,
    pub is_dir: bool,
    pub dir_cache: Option<Vec<u8>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SocketSnapshot {
    pub sock_kind: crate::fd::SocketKind,
    pub nonblock: bool,
    pub bound: Option<SockAddr>,
    /// Recorded for fidelity; the OS picks the actual backlog on restore.
    pub listen_backlog: Option<LeI32>,
    pub so_reuseaddr: bool,
    pub so_keepalive: bool,
    pub tcp_nodelay: bool,
    pub peer_addr_present: bool,
    pub last_error: LeI32,
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
    pub self_event_fd: Option<LeU32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EpollEntrySnapshot {
    pub fd: LeU32,
    pub events: LeU32,
    pub data: LeU64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct EventFdSnapshot {
    pub counter: LeU64,
    pub nonblock: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct LinearAllocatorSnapshot {
    /// Identical shape to `crate::mm::Arena`; we serialize the runtime
    /// type directly because `Arena` already derives the right traits.
    pub arenas: Vec<crate::mm::Arena>,
    pub high_water: LeU32,
}

/// Identical-shape mirror of `crate::kernel::ClockState` so the runtime
/// type doesn't need a serde dep transitively. `apply_snapshot` writes
/// from this into the kernel's `ClockState`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ClockStateSnapshot {
    pub boot_monotonic_ns: LeU64,
}

/// Identical-shape mirror of `crate::sys::signal::SignalState`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SignalStateSnapshot {
    pub actions: std::collections::BTreeMap<i32, crate::sys::signal::SigAction>,
    pub mask: LeU64,
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
            mask: LeU64(s.mask),
            alt_stack: s.alt_stack.clone(),
        }
    }
}

impl From<&crate::kernel::ClockState> for ClockStateSnapshot {
    fn from(c: &crate::kernel::ClockState) -> Self {
        Self {
            boot_monotonic_ns: LeU64(c.boot_monotonic_ns),
        }
    }
}

#[derive(Debug)]
pub enum SnapshotError {
    /// Snapshot format version mismatch. D-series bumps the version
    /// when the schema changes incompatibly.
    FormatVersionMismatch {
        found: u32,
        supported: u32,
    },
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
/// form. Runtime handles (`Arc<Notify>`, raw fds) are dropped — they
/// are rebuilt on `apply_snapshot`. The Wasmtime `Memory` handle is
/// retained on the kernel but the bytes are read out into the snapshot's
/// `pages` overlay here; `apply_snapshot` grows + chunk-copies them on
/// restore.
///
/// **Concurrency:** the caller must serialize against guest execution —
/// this runtime is single-threaded (`wasm_threads` is off and the tokio
/// runtime is current-thread), so the freeze CLI's quiescent-point
/// ordering is sufficient; if you ever change that, gate this function
/// behind an explicit barrier.
pub fn try_to_snapshot(
    kernel: &Kernel,
    store: &impl wasmtime::AsContext,
) -> Result<KernelSnapshot, SnapshotError> {
    // Read linear memory (sparse, only non-zero pages).
    let pages = if let Ok(mem) = kernel.memory() {
        let data = mem.data(store.as_context());
        collect_pages(data)
    } else {
        // No memory attached yet — emit an empty pages list. `apply_snapshot`
        // will refuse to restore an empty-pages snapshot onto a no-memory
        // target; this is a host-construction error caught by the freeze CLI.
        Vec::new()
    };

    Ok(build_kernel_snapshot(kernel, pages))
}

/// Build a `KernelSnapshot` *without* reading linear memory. Used by
/// unit tests that don't have a `Store<Kernel>` to pass in. D2.4:
/// `try_to_snapshot` calls this after collecting `pages` from memory.
fn build_kernel_snapshot(kernel: &Kernel, pages: Vec<MemoryPageSnapshot>) -> KernelSnapshot {
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
            Resource::PipeRead(p) => {
                ResourceSnapshot::from_pipe(ResourceKind::PipeRead, p.snapshot())
            }
            Resource::PipeWrite(p) => {
                ResourceSnapshot::from_pipe(ResourceKind::PipeWrite, p.snapshot())
            }
            Resource::File(shared) => {
                let guard = shared.lock();
                ResourceSnapshot::from_file(crate::snapshot::FileSnapshot {
                    path: guard.path.clone(),
                    pos: LeU64(guard.pos),
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
        entries.push(FdEntrySnapshot {
            fd: LeU32(fd),
            kind,
        });
    }

    let cloexec: Vec<LeU32> = {
        let mut v: Vec<LeU32> = kernel
            .fds
            .iter_cloexec_for_snapshot()
            .into_iter()
            .map(LeU32)
            .collect();
        v.sort();
        v
    };

    KernelSnapshot {
        format_version: LeU32(SNAPSHOT_FORMAT_VERSION),
        pages,
        fds: crate::snapshot::FdSnapshot {
            entries,
            next_fd: LeU32(kernel.fds.next_fd_for_snapshot()),
            cloexec,
        },
        mm: kernel.mm.snapshot(),
        vfs: crate::snapshot::VfsSnapshot::from(&kernel.vfs),
        clock: crate::snapshot::ClockStateSnapshot::from(&kernel.clock),
        brk: LeU32(kernel.brk),
        args: kernel.args.clone(),
        env: kernel.env.clone(),
        rng_seed: kernel.rng_seed,
        signals: crate::snapshot::SignalStateSnapshot::from(&kernel.signals),
        exit_code: kernel.exit_code.map(LeI32),
        comm: kernel.comm,
    }
}

/// P2-D2 / ADR 0002 §3: scan a linear-memory blob in 64 KiB page
/// chunks and return a sparse `Vec<MemoryPageSnapshot>` containing
/// only the pages whose bytes are not all zero. Untouched pages are
/// restored on `apply_snapshot` by `Memory::grow` (which zero-fills
/// new pages) and then optionally chunk-copied with the pages in
/// the snapshot — the result is byte-identical to the pre-snapshot
/// memory.
pub fn collect_pages(data: &[u8]) -> Vec<MemoryPageSnapshot> {
    let mut pages = Vec::new();
    let mut chunks = data.chunks_exact(PAGE_SIZE_BYTES);
    let mut page_index: u32 = 0;
    for chunk in &mut chunks {
        if !is_zero_page(chunk) {
            pages.push(MemoryPageSnapshot {
                page_index: LeU32(page_index),
                bytes: chunk.to_vec(),
            });
        }
        page_index += 1;
    }
    let remainder = chunks.remainder();
    if !remainder.is_empty() && !is_zero_page(remainder) {
        pages.push(MemoryPageSnapshot {
            page_index: LeU32(page_index),
            bytes: remainder.to_vec(),
        });
    }
    pages
}

fn is_zero_page(page: &[u8]) -> bool {
    // `iter().any(|b| *b != 0)` is faster than full equality for sparse non-zero pages.
    !page.iter().any(|b| *b != 0)
}

/// Apply a `KernelSnapshot` to a target `Kernel`. The target kernel is
/// expected to be freshly constructed via `Kernel::new_without_stdio`
/// (D3 the freeze CLI owns that flow). The function:
///
/// - replaces `Kernel.{args, env, comm, exit_code, brk, vfs, clock, signals,
///   rng_seed, rng}` from the snapshot;
/// - replaces `Kernel.mm` via `replace_from_snapshot`;
/// - drains `Kernel.fds` (closes any stdio) and rebuilds it from the
///   snapshot: pipes (Stdin/Stdout/Stderr/PipeRead/PipeWrite), File
///   (reopened by path), EventFd (counter reset), Epoll (rebuilt
///   freshly);
/// - grows the attached linear memory to fit the highest `page_index`
///   in `snap.pages` (or stays at its current size if `pages` is empty)
///   and chunk-copies each saved page's bytes into the right slot.
///
/// Sockets, accepted streams, and abstract unix namespaces return
/// `SnapshotError::{AcceptedStreamOnListener, AbstractUnixNamespace}`
/// so the freeze CLI can abort cleanly.
pub fn apply_snapshot(
    snap: KernelSnapshot,
    kernel: &mut Kernel,
    store: &mut impl wasmtime::AsContextMut<Data = Kernel>,
) -> Result<(), SnapshotError> {
    apply_snapshot_kernel_state(&snap, kernel)?;
    apply_snapshot_to_memory_inner(&snap, kernel, store)
}

/// Step 1 of `apply_snapshot`: replace kernel-resident state (args,
/// env, brk, fd-table, signals, vfs, clock, mm, rng). Takes only
/// `&mut Kernel` — does NOT touch `Store<Kernel>`. Public for callers
/// that want to drive the apply by hand (specifically: tests that
/// hit Rust's borrow checker when holding both `&mut Kernel` (via
/// `Store::data_mut`) and `&mut Store<Kernel>` at the same time).
pub fn apply_snapshot_kernel_state(
    snap: &KernelSnapshot,
    kernel: &mut Kernel,
) -> Result<(), SnapshotError> {
    if snap.format_version.0 != SNAPSHOT_FORMAT_VERSION {
        return Err(SnapshotError::FormatVersionMismatch {
            found: snap.format_version.0,
            supported: SNAPSHOT_FORMAT_VERSION,
        });
    }
    kernel.args = snap.args.clone();
    kernel.env = snap.env.clone();
    kernel.comm = snap.comm;
    kernel.exit_code = snap.exit_code.map(|c| c.0);
    kernel.brk = snap.brk.0;
    kernel.rng_seed = snap.rng_seed;
    kernel.rng = rand::rngs::SmallRng::from_seed(kernel.rng_seed);
    kernel.vfs = Vfs {
        root: snap.vfs.root.clone(),
        cwd: snap.vfs.cwd.clone(),
    };
    kernel.clock = crate::kernel::ClockState {
        boot_monotonic_ns: snap.clock.boot_monotonic_ns.0,
    };
    let mut actions: std::collections::HashMap<i32, crate::sys::signal::SigAction> =
        std::collections::HashMap::new();
    for (k, v) in &snap.signals.actions {
        actions.insert(*k, *v);
    }
    kernel.signals = SignalState {
        actions,
        mask: snap.signals.mask.0,
        alt_stack: snap.signals.alt_stack.clone(),
    };
    kernel.mm.replace_from_snapshot(snap.mm.clone());

    // ---- fd table ---------------------------------------------------------
    use crate::fd::{EpollInner, EventFdInner, PipeRead, PipeWrite, Resource, SharedFilePos};
    use crate::sys::file::FilePos;
    use std::collections::HashMap;
    use std::sync::atomic::AtomicBool;

    // Drain and rebuild fd table from scratch.
    kernel.fds = FdTable::empty();

    // Sort entries by fd for deterministic rebuild order.
    let mut entries = snap.fds.entries.clone();
    entries.sort_by_key(|e| e.fd.0);

    // Track which fds become stdin/stdout/stderr so we can hydrate the
    // matching output buffers when the host driver queries them later.
    for entry in &entries {
        let fd_num = entry.fd;
        let kind = entry.kind.kind;
        let body = &entry.kind.body;
        let resource: Resource = match kind {
            ResourceKind::Stdin => {
                let snap = body.pipe.clone().ok_or(SnapshotError::UnknownResource)?;
                Resource::Stdin(PipeRead {
                    buf: Arc::new(parking_lot::Mutex::new(snap.buf)),
                    closed: Arc::new(parking_lot::Mutex::new(snap.closed)),
                    nonblock: Arc::new(AtomicBool::new(snap.nonblock)),
                    notify: Arc::new(tokio::sync::Notify::new()),
                })
            }
            ResourceKind::Stdout => {
                let snap = body.pipe.clone().ok_or(SnapshotError::UnknownResource)?;
                Resource::Stdout(PipeWrite {
                    buf: Arc::new(parking_lot::Mutex::new(snap.buf)),
                    closed: Arc::new(parking_lot::Mutex::new(snap.closed)),
                    nonblock: Arc::new(AtomicBool::new(snap.nonblock)),
                    notify: Arc::new(tokio::sync::Notify::new()),
                })
            }
            ResourceKind::Stderr => {
                let snap = body.pipe.clone().ok_or(SnapshotError::UnknownResource)?;
                Resource::Stderr(PipeWrite {
                    buf: Arc::new(parking_lot::Mutex::new(snap.buf)),
                    closed: Arc::new(parking_lot::Mutex::new(snap.closed)),
                    nonblock: Arc::new(AtomicBool::new(snap.nonblock)),
                    notify: Arc::new(tokio::sync::Notify::new()),
                })
            }
            ResourceKind::PipeRead => {
                let snap = body.pipe.clone().ok_or(SnapshotError::UnknownResource)?;
                Resource::PipeRead(PipeRead {
                    buf: Arc::new(parking_lot::Mutex::new(snap.buf)),
                    closed: Arc::new(parking_lot::Mutex::new(snap.closed)),
                    nonblock: Arc::new(AtomicBool::new(snap.nonblock)),
                    notify: Arc::new(tokio::sync::Notify::new()),
                })
            }
            ResourceKind::PipeWrite => {
                let snap = body.pipe.clone().ok_or(SnapshotError::UnknownResource)?;
                Resource::PipeWrite(PipeWrite {
                    buf: Arc::new(parking_lot::Mutex::new(snap.buf)),
                    closed: Arc::new(parking_lot::Mutex::new(snap.closed)),
                    nonblock: Arc::new(AtomicBool::new(snap.nonblock)),
                    notify: Arc::new(tokio::sync::Notify::new()),
                })
            }
            ResourceKind::File => {
                let fsnap = body.file.clone().ok_or(SnapshotError::UnknownResource)?;
                if fsnap.is_dir {
                    return Err(SnapshotError::Unsupported(
                        "Resource::File (directory) reopen on apply_snapshot",
                    ));
                }
                let path = fsnap.path.clone().ok_or_else(|| {
                    SnapshotError::MissingPath("<unknown: no path captured>".into())
                })?;
                let mut f = std::fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open(&path)
                    .map_err(|e| SnapshotError::IoError(e, format!("open {}", path.display())))?;
                use std::io::{Seek, SeekFrom};
                f.seek(SeekFrom::Start(fsnap.pos.0))
                    .map_err(|e| SnapshotError::IoError(e, format!("seek {}", path.display())))?;
                let mut fp = FilePos::new(f);
                fp.path = Some(path);
                fp.pos = fsnap.pos.0;
                fp.dir_cache = fsnap.dir_cache;
                Resource::File(SharedFilePos::new(parking_lot::Mutex::new(fp)))
            }
            ResourceKind::Socket => {
                return Err(SnapshotError::Unsupported(
                    "Resource::Socket reopen on apply_snapshot (D3 freeze CLI decides policy)",
                ));
            }
            ResourceKind::Epoll => {
                let esnap = body.epoll.clone().ok_or(SnapshotError::UnknownResource)?;
                let entries_map: HashMap<u32, crate::fd::EpollEntry> = esnap
                    .entries
                    .iter()
                    .map(|e| {
                        (
                            e.fd.0,
                            crate::fd::EpollEntry {
                                fd: e.fd.0,
                                events: e.events.0,
                                data: e.data.0,
                                wake: Arc::new(tokio::sync::Notify::new()),
                            },
                        )
                    })
                    .collect();
                Resource::Epoll(EpollInner {
                    entries: parking_lot::Mutex::new(entries_map),
                    cancel: Arc::new(tokio::sync::Notify::new()),
                    self_event_fd: esnap.self_event_fd.map(|f| f.0),
                })
            }
            ResourceKind::EventFd => {
                let esnap = body.eventfd.clone().ok_or(SnapshotError::UnknownResource)?;
                Resource::EventFd(EventFdInner {
                    counter: parking_lot::Mutex::new(esnap.counter.0),
                    notify: Arc::new(tokio::sync::Notify::new()),
                    nonblock: AtomicBool::new(esnap.nonblock),
                })
            }
        };
        kernel.fds.insert_at(fd_num.0, resource).map_err(|e| {
            SnapshotError::IoError(
                std::io::Error::other(format!("fd {} conflict: {e}", fd_num.0)),
                "fd insert_at".into(),
            )
        })?;
    }

    for fd in &snap.fds.cloexec {
        kernel.fds.set_cloexec(fd.0, true);
    }
    kernel.fds.set_next_fd_for_snapshot(snap.fds.next_fd.0);
    Ok(())
}

/// Step 2 of `apply_snapshot`: grow the attached linear memory to
/// fit the highest `page_index` in `snap.pages` (or stay at its
/// current size if `pages` is empty) and chunk-copy each saved page's
/// bytes into the right slot. Idempotent: if the snapshot carries no
/// pages, this is a no-op (the memory remains at whatever size the
/// target store was born with — sufficient for the roundtrip test
/// fixture, which builds a 32-page module ahead of time).
fn apply_snapshot_to_memory_inner(
    snap: &KernelSnapshot,
    kernel: &mut Kernel,
    store: &mut impl wasmtime::AsContextMut<Data = Kernel>,
) -> Result<(), SnapshotError> {
    if snap.pages.is_empty() {
        return Ok(());
    }
    // Clone the `Memory` handle out of kernel (it's `Copy` per
    // `src/mem.rs:28`). Drop the kernel borrow immediately so we can
    // re-borrow mutably through `store`.
    let mem = kernel
        .memory()
        .map_err(|e| {
            SnapshotError::IoError(
                std::io::Error::other(format!("memory not attached: errno={e}")),
                "kernel.memory()".into(),
            )
        })?
        .clone();
    apply_snapshot_to_memory(snap, mem, store)
}

/// Memory-only restore driver. Takes the cloned `Memory` handle
/// instead of a `&mut Kernel` — disjoint borrow of `store`, no kernel
/// reference. Use this directly from tests to avoid the
/// `&mut kernel + &mut store` overlap that Rust's NLL rejects on
/// inline expressions.
///
/// **Public** so integration tests can drive the apply by hand
/// (also see [`apply_snapshot_kernel_state`] for the kernel-only
/// half of the same operation).
pub fn apply_snapshot_to_memory(
    snap: &KernelSnapshot,
    mem: wasmtime::Memory,
    store: &mut impl wasmtime::AsContextMut<Data = Kernel>,
) -> Result<(), SnapshotError> {
    if snap.pages.is_empty() {
        return Ok(());
    }
    // How many wasm pages does this snapshot need?
    let target_pages = snap
        .pages
        .iter()
        .map(|p| p.page_index.0 as usize)
        .max()
        .unwrap()
        + 1;
    let cur_pages: usize = mem.data_size(&store.as_context()) / PAGE_SIZE_BYTES;
    if target_pages > cur_pages {
        let delta: u64 = (target_pages - cur_pages) as u64;
        mem.grow(store.as_context_mut(), delta).map_err(|e| {
            SnapshotError::IoError(
                std::io::Error::other(format!("memory.grow failed: {e:?}")),
                "mem.grow".into(),
            )
        })?;
    }

    // chunk-copy each page's bytes into the right slot.
    let bytes = mem.data_mut(store.as_context_mut());
    for page in &snap.pages {
        let start = page.page_index.0 as usize * PAGE_SIZE_BYTES;
        let end = start + page.bytes.len();
        if end > bytes.len() {
            return Err(SnapshotError::IoError(
                std::io::Error::other(format!(
                    "page {} out of bounds: {end} > {}",
                    page.page_index.0,
                    bytes.len()
                )),
                "page chunk-copy".into(),
            ));
        }
        bytes[start..end].copy_from_slice(&page.bytes);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{build_engine, build_store};
    use rand::RngCore;
    use std::sync::Arc;
    use wasmtime::{Linker, Store};

    /// Helper: parse + compile a WAT string with the kernel's test engine.
    fn compile_wat(engine: &wasmtime::Engine, wat: &str) -> anyhow::Result<wasmtime::Module> {
        let bytes = wat::parse_str(wat)?;
        Ok(wasmtime::Module::new(engine, &bytes)?)
    }

    #[test]
    fn format_version_constant_value() {
        assert_eq!(SNAPSHOT_FORMAT_VERSION, 1);
    }

    #[test]
    fn smoke_postcard_roundtrip_of_format_version_only() {
        // Encode a minimal snapshot via postcard and decode it back.
        let snap = KernelSnapshot {
            format_version: LeU32(SNAPSHOT_FORMAT_VERSION),
            pages: vec![],
            fds: FdSnapshot::default(),
            mm: LinearAllocatorSnapshot::default(),
            vfs: VfsSnapshot {
                root: "/".into(),
                cwd: "/".into(),
            },
            clock: ClockStateSnapshot::default(),
            brk: LeU32(0),
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
        assert_eq!(back.format_version, LeU32(1));
        assert!(back.pages.is_empty());
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
            high_water: LeU32(0x1_001_0000),
        };
        let bytes = postcard::to_stdvec(&lsnap).expect("encode");
        let back: LinearAllocatorSnapshot = postcard::from_bytes(&bytes).expect("decode");
        // Field-by-field checks; PartialEq was dropped to avoid Eq-bound
        // chains through SocketAddr-free paths.
        assert_eq!(back.high_water, LeU32(0x1_001_0000));
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
        let snap = build_kernel_snapshot(&kernel, vec![]);

        // Header.
        assert_eq!(snap.format_version, LeU32(SNAPSHOT_FORMAT_VERSION));
        assert_eq!(snap.brk, LeU32(0x1000));
        assert_eq!(snap.rng_seed, [42u8; 32]);
        assert_eq!(
            snap.args,
            vec!["edge-python".to_string(), "main.py".to_string()]
        );
        assert_eq!(snap.env, vec![("PATH".to_string(), "/usr/bin".to_string())]);
        assert_eq!(snap.comm, kernel.comm);

        // FDs: 0 (stdin), 1 (stdout), 2 (stderr), 3 (eventfd) all present.
        assert_eq!(snap.fds.entries.len(), 4);
        let fds: Vec<u32> = snap.fds.entries.iter().map(|e| e.fd.0).collect();
        assert_eq!(fds, vec![0, 1, 2, 3]);

        // Specifically the EventFd entry has counter=7.
        let efd_entry = snap.fds.entries.iter().find(|e| e.fd == LeU32(3)).unwrap();
        assert_eq!(efd_entry.kind.kind, ResourceKind::EventFd);
        let efd = efd_entry.kind.body.eventfd.as_ref().expect("eventfd body");
        assert_eq!(efd.counter, LeU64(7));
        assert!(!efd.nonblock);

        // next_fd should be ≥ 4.
        assert!(snap.fds.next_fd.0 >= 4);

        // Round-trip the entire snapshot via postcard.
        let bytes = postcard::to_stdvec(&snap).expect("encode succeeds");
        let back: KernelSnapshot = postcard::from_bytes(&bytes).expect("decode succeeds");
        assert_eq!(back.format_version, snap.format_version);
        assert_eq!(back.brk, snap.brk);
        assert_eq!(back.rng_seed, snap.rng_seed);
        assert_eq!(back.args, snap.args);
        assert_eq!(back.env, snap.env);
        assert_eq!(back.fds.entries.len(), snap.fds.entries.len());
        assert_eq!(back.mm.high_water, snap.mm.high_water);

        // Round-trip the entire snapshot via postcard.
        let bytes = postcard::to_stdvec(&snap).expect("encode succeeds");
        let back: KernelSnapshot = postcard::from_bytes(&bytes).expect("decode succeeds");
        assert_eq!(back.format_version, snap.format_version);
        assert_eq!(back.brk, snap.brk);
        assert_eq!(back.rng_seed, snap.rng_seed);
        assert_eq!(back.args, snap.args);
        assert_eq!(back.env, snap.env);
        assert_eq!(back.fds.entries.len(), snap.fds.entries.len());
        assert_eq!(back.mm.high_water, snap.mm.high_water);
    }

    #[test]
    fn apply_snapshot_restores_linear_memory() {
        // Write a 16-byte pattern at offset 0x100 in store A; restore
        // to store B; verify the pattern survives. The fresh
        // store's 1-page (64 KiB) memory is large enough for the 16
        // bytes; no grow needed.
        use crate::snapshot::endian::LeU32;
        let (mut store_a, _mem) = fresh_store_with_mem();
        {
            let mem = kernel_memory(&store_a);
            let bytes = mem.data_mut(&mut store_a);
            bytes[0x100..0x110].copy_from_slice(b"0123456789ABCDEF");
        }
        let snap = {
            let kernel = store_a.data();
            try_to_snapshot(kernel, &store_a).expect("snapshot")
        };

        // Apply to a fresh store B via apply_with_store; the
        // verifier checks memory bytes match.
        apply_with_store(snap, |_target, store_b| {
            let mem = kernel_memory(store_b);
            let bytes = mem.data(&*store_b);
            assert_eq!(
                &bytes[0x100..0x110],
                b"0123456789ABCDEF",
                "linear memory not restored byte-exact"
            );
            assert!(
                bytes[..0x100].iter().all(|b| *b == 0),
                "pre-pattern bytes must be zero"
            );
        })
        .expect("apply_snapshot");
    }

    #[test]
    fn apply_snapshot_grows_memory() {
        // Fresh store has 1 page (64 KiB). Write a recognisable
        // pattern at page index 5, snapshot, then apply onto a
        // fresh 1-page store B. After apply, store must have grown
        // to >= 6 pages and the bytes at page 5 must match.
        let (mut store_a, _mem) = fresh_store_with_mem();
        {
            let mem = kernel_memory(&store_a);
            mem.grow(&mut store_a, 5).expect("grow store_a");
            let bytes = mem.data_mut(&mut store_a);
            let p5_start = 5 * PAGE_SIZE_BYTES;
            bytes[p5_start..p5_start + 16].copy_from_slice(b"GROW_GROWS_PAGE5");
        }
        let snap = {
            let kernel = store_a.data();
            try_to_snapshot(kernel, &store_a).expect("snapshot")
        };
        assert_eq!(snap.pages.len(), 1);
        assert_eq!(snap.pages[0].page_index, LeU32(5));

        apply_with_store(snap, |_target, store_b| {
            let mem = kernel_memory(store_b);
            assert!(
                mem.data_size(&*store_b) >= 6 * PAGE_SIZE_BYTES,
                "memory must have grown to >= 6 pages"
            );
            let bytes = mem.data(&*store_b);
            let p5_start = 5 * PAGE_SIZE_BYTES;
            assert_eq!(
                &bytes[p5_start..p5_start + 16],
                b"GROW_GROWS_PAGE5",
                "page 5 contents must roundtrip"
            );
        })
        .expect("apply_snapshot");
    }

    /// Extract the `Memory` handle out of a Store (Copy per `src/mem.rs:28`).
    fn kernel_memory(store: &Store<Kernel>) -> wasmtime::Memory {
        store.data().memory().expect("memory attached").clone()
    }

    #[test]
    fn apply_snapshot_rebuilds_eventfd_and_stdio() {
        // Build a kernel with stdio + an EventFd at fd 3, snapshot it,
        // apply to a fresh kernel, and verify trivial fields plus the
        // EventFd entry survives. This is the D1 verification path
        // (the linear-memory roundtrip lands in D2).
        use crate::fd::{EventFdInner, Resource};
        use std::sync::atomic::AtomicBool;

        let kernel = Kernel::new_without_stdio(
            vec!["edge-python".into(), "main.py".into()],
            vec![("PATH".to_string(), "/usr/bin".to_string())],
        );
        let mut kernel = kernel;
        kernel.rng_seed = [9u8; 32];
        kernel.rng = rand::rngs::SmallRng::from_seed(kernel.rng_seed);
        kernel.brk = 0x2000;
        kernel.comm = *b"edge-libos\0\0\0\0\0\0";

        let efd_fd = kernel.fds.insert(Resource::EventFd(EventFdInner {
            counter: parking_lot::Mutex::new(11),
            notify: Arc::new(tokio::sync::Notify::new()),
            nonblock: AtomicBool::new(true),
        }));
        assert_eq!(efd_fd, 3);

        let snap = build_kernel_snapshot(&kernel, vec![]);

        // D2.5: apply needs a real Store<Kernel> with attached linear
        // memory. `apply_with_store` runs apply_snapshot, then hands
        // back immutable `&Kernel` + `&Store<Kernel>` to the verify
        // closure — once the closure returns, both refs are dropped.
        let expected_comm = kernel.comm;
        let expected_args = vec!["edge-python".to_string(), "main.py".to_string()];
        let expected_env = vec![("PATH".to_string(), "/usr/bin".to_string())];
        apply_with_store(snap, |target, _store| {
            // Trivial fields restored.
            assert_eq!(target.brk, 0x2000);
            assert_eq!(target.rng_seed, [9u8; 32]);
            assert_eq!(target.args, expected_args);
            assert_eq!(target.env, expected_env);
            assert_eq!(target.comm, expected_comm);
            assert_eq!(target.exit_code, None);

            // fd table: 0/1/2 stdio + 3 eventfd all present.
            assert!(target.fds.contains(0));
            assert!(target.fds.contains(1));
            assert!(target.fds.contains(2));
            assert!(target.fds.contains(3));

            // Verify the EventFd counter survived.
            match target.fds.get(3).expect("fd 3 present") {
                Resource::EventFd(e) => {
                    assert_eq!(*e.counter.lock(), 11);
                    assert!(e.nonblock.load(std::sync::atomic::Ordering::Relaxed));
                }
                Resource::Stdin(_) => panic!("expected EventFd at fd 3, got Stdin"),
                Resource::Stdout(_) => panic!("expected EventFd at fd 3, got Stdout"),
                Resource::Stderr(_) => panic!("expected EventFd at fd 3, got Stderr"),
                Resource::PipeRead(_) => panic!("expected EventFd at fd 3, got PipeRead"),
                Resource::PipeWrite(_) => panic!("expected EventFd at fd 3, got PipeWrite"),
                Resource::File(_) => panic!("expected EventFd at fd 3, got File"),
                Resource::Socket(_) => panic!("expected EventFd at fd 3, got Socket"),
                Resource::Epoll(_) => panic!("expected EventFd at fd 3, got Epoll"),
            }

            // next_fd bumped to 4 (matching snapshot).
            assert_eq!(target.fds.next_fd_for_snapshot(), 4);
        })
        .expect("apply_snapshot");
    }

    #[test]
    fn apply_snapshot_rejects_socket_variant() {
        // Build a snapshot with a Socket variant directly (we cannot
        // easily make a TcpListener here without binding a port). The
        // expected outcome: apply_snapshot returns Unsupported.
        let snap = KernelSnapshot {
            format_version: LeU32(SNAPSHOT_FORMAT_VERSION),
            fds: FdSnapshot {
                entries: vec![FdEntrySnapshot {
                    fd: LeU32(3),
                    kind: ResourceSnapshot::from_socket(SocketSnapshot::default()),
                }],
                next_fd: LeU32(4),
                cloexec: vec![],
            },
            ..make_test_snapshot()
        };
        let err = apply_with_store(snap, |_target, _store| ())
            .expect_err("apply_snapshot should reject Socket variant");
        assert!(matches!(err, SnapshotError::Unsupported(_)), "got {err:?}");
    }

    #[test]
    fn collect_pages_skips_zero_pages() {
        // Build a 128 KiB buffer: page 0 all-zero, page 1 has bytes
        // [b'x', 0, 0, ...]. collect_pages must return exactly one entry
        // at LeU32(1). The remainder branch is exercised by the partial-
        // page case below.
        let mut data = vec![0u8; 2 * PAGE_SIZE_BYTES];
        // Page 1: byte 0 = b'x'; the rest stays zero.
        data[PAGE_SIZE_BYTES] = b'x';
        let pages = collect_pages(&data);
        assert_eq!(pages.len(), 1, "page 0 (zero) must be omitted");
        assert_eq!(pages[0].page_index, LeU32(1));
        assert_eq!(pages[0].bytes[0], b'x');

        // Partial tail (e.g. 70 KiB) — the first 64 KiB are zero,
        // the remaining 6144 bytes are non-zero.
        let mut data2 = vec![0u8; 64 * 1024 + 6144];
        for i in (64 * 1024)..data2.len() {
            data2[i] = ((i & 0xFF) as u8).wrapping_add(1);
        }
        let pages2 = collect_pages(&data2);
        assert_eq!(pages2.len(), 1, "partial-tail non-zero must be one entry");
        assert_eq!(pages2[0].page_index, LeU32(1));
        assert_eq!(pages2[0].bytes.len(), 6144);
    }

    /// Build a 1-page linear-memory WAT module, instantiate it via a
    /// fresh `Store<Kernel>`, attach memory, and return the store. Used
    /// by the D2.5 `apply_snapshot_*` tests that need a real
    /// `wasmtime::Memory` for grow + chunk-copy.
    ///
    /// Synchronous: drives the wasmtime async instantiate via
    /// `futures::executor::block_on` — fine for tests; never used at
    /// runtime.
    fn fresh_store_with_mem() -> (Store<Kernel>, wasmtime::Memory) {
        use wasmtime::AsContextMut;
        let engine = build_engine().expect("engine");
        let wat = r#"
            (module
                (memory (export "memory") 1)
                (func (export "_start") (nop))
            )
        "#;
        let module = compile_wat(&engine, wat).expect("compile wat");
        let mut store = build_store(&engine, Kernel::new_without_stdio(vec![], vec![]));
        let instance = futures::executor::block_on(
            Linker::<Kernel>::new(&engine).instantiate_async(store.as_context_mut(), &module),
        )
        .expect("instantiate");
        let mem = instance
            .get_memory(&mut store, "memory")
            .expect("memory export");
        store.data_mut().attach_memory(mem);
        (store, mem)
    }

    /// Helper: run `apply_snapshot(snap, ...)` against a fresh
    /// `Store<Kernel>` (1-page WAT module attached). Returns the
    /// `Result` from `apply_snapshot`. The caller passes a verifying
    /// closure that runs assertions against the now-loaded kernel +
    /// store; that closure takes `&Kernel, &Store<Kernel>` and
    /// borrows both immutably (so the kernel + store are not aliased
    /// mutably while assertions run).
    fn apply_with_store<R>(
        snap: KernelSnapshot,
        verify: impl FnOnce(&Kernel, &Store<Kernel>) -> R,
    ) -> Result<R, SnapshotError> {
        // Step 1: kernel-state only (no Store borrow needed).
        let (mut store, mem) = fresh_store_with_mem();
        {
            let kernel = store.data_mut();
            apply_snapshot_kernel_state(&snap, kernel)?;
        }
        // Step 2: linear memory via the cloned `Memory` handle. No
        // `&mut Kernel` borrow active — only `&mut store` and the
        // Memory handle, which is `Copy`.
        apply_snapshot_to_memory(&snap, mem, &mut store)?;
        Ok(verify(store.data(), &store))
    }

    #[test]
    fn memory_page_snapshot_roundtrip() {
        // Build a Vec<MemoryPageSnapshot> with 2 entries, encode/decode,
        // verify page_index and bytes match exactly. Confirms ADR 0002 §3
        // wire framing: LeU32 page_index, then 64 KiB of bytes per entry.
        let entry_a = MemoryPageSnapshot {
            page_index: LeU32(3),
            bytes: vec![0xAB; PAGE_SIZE_BYTES],
        };
        let mut bytes_b = vec![0u8; PAGE_SIZE_BYTES];
        for (i, b) in bytes_b.iter_mut().enumerate() {
            *b = (i & 0xFF) as u8;
        }
        let entry_b = MemoryPageSnapshot {
            page_index: LeU32(17),
            bytes: bytes_b,
        };
        let pages = vec![entry_a.clone(), entry_b.clone()];

        let bytes = postcard::to_stdvec(&pages).expect("encode pages");
        let back: Vec<MemoryPageSnapshot> = postcard::from_bytes(&bytes).expect("decode pages");
        assert_eq!(back.len(), 2);
        assert_eq!(back[0].page_index, LeU32(3));
        assert_eq!(back[0].bytes.len(), PAGE_SIZE_BYTES);
        assert_eq!(back[0].bytes[0], 0xAB);
        assert_eq!(back[0].bytes[PAGE_SIZE_BYTES - 1], 0xAB);
        assert_eq!(back[1].page_index, LeU32(17));
        assert_eq!(back[1].bytes.len(), PAGE_SIZE_BYTES);
        assert_eq!(back[1].bytes[123], 123);
        // Bytes match exactly (whole-buffer equality).
        assert_eq!(back[0].bytes, entry_a.bytes);
        assert_eq!(back[1].bytes, entry_b.bytes);
    }

    fn make_test_snapshot() -> KernelSnapshot {
        KernelSnapshot {
            format_version: LeU32(SNAPSHOT_FORMAT_VERSION),
            pages: vec![],
            fds: FdSnapshot::default(),
            mm: LinearAllocatorSnapshot::default(),
            vfs: VfsSnapshot {
                root: "/".into(),
                cwd: "/".into(),
            },
            clock: ClockStateSnapshot::default(),
            brk: LeU32(0),
            args: vec![],
            env: vec![],
            rng_seed: [0u8; 32],
            signals: SignalStateSnapshot::default(),
            exit_code: None,
            comm: [0u8; 16],
        }
    }

    #[test]
    fn apply_snapshot_rejects_format_mismatch() {
        let mut snap = make_test_snapshot();
        snap.format_version = LeU32(99);
        let err = apply_with_store(snap, |_target, _store| ())
            .expect_err("apply should reject mismatched format_version");
        match err {
            SnapshotError::FormatVersionMismatch { found, supported } => {
                assert_eq!(found, 99);
                assert_eq!(supported, SNAPSHOT_FORMAT_VERSION);
            }
            other => panic!("expected FormatVersionMismatch, got {other:?}"),
        }
    }
}
