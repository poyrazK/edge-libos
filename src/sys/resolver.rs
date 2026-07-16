//! `getaddrinfo(3)` replacement — project-private NR_RESOLVE (NR 400).
//!
//! Why this exists: Linux x86-64 has no `getaddrinfo` syscall (NR 63 is
//! `uname`). musl's libc implements `getaddrinfo` by issuing ordinary
//! `socket` / `sendto` / `recvmsg` / `poll` syscalls over UDP. Our kernel
//! has no UDP socket layer today ([`crate::sys::socket`] only handles
//! TCP), so musl's resolver path can't run end-to-end.
//!
//! Path B (this module) sidesteps the missing UDP layer entirely: we
//! expose a single project-private syscall that carries `getaddrinfo`'s
//! semantics, and a tiny guest-side libc adapter in
//! `guest/resolver/` overrides musl's `getaddrinfo` /
//! `freeaddrinfo` symbols to marshal through it. See ADR 0007.
//!
//! ## NR choice
//!
//! NR_RESOLVE = **400**. The upstream
//! `arch/x86/entry/syscalls/syscall_64.tbl` header reserves the range
//! 387–423 ("don't use numbers 387 through 423, add new calls after
//! the last 'common' entry"). NR 400 sits inside that reserved range
//! and is guaranteed never to collide with any current or future
//! upstream syscall. Same justification as NR_SNAPSHOT = 123, which
//! sits adjacent to upstream `setfsuid` — the project-private
//! contract is "a hole the kernel won't fill," not "an
//! upstream-unused NR."
//!
//! ## Return convention
//!
//! - `>= 0` — success; the count of `addrinfo` nodes written.
//! - `<  0` — `-EAI_*` (negative, matching musl's `netdb.h`):
//!   `-1 BADFLAGS`, `-2 NONAME`, `-3 AGAIN`, `-4 FAIL`,
//!   `-6 FAMILY`, `-7 SOCKTYPE`, `-8 SERVICE`, `-10 MEMORY`,
//!   `-11 SYSTEM`, `-12 OVERFLOW`. The guest-side adapter returns the
//!   syscall return cast to `int` directly — both sides share the
//!   same negative EAI space, no translation.
//!
//! ## Lock discipline
//!
//! Per ADR 0001 §2 + 0006 §3: acquire the parking_lot lock, clone out
//! any Arc the call needs (the resolver backend, cache entry), drop
//! the lock before any `.await`. Re-acquire after `.await` to insert
//! cache + apply denylist filter. Never hold a `parking_lot::Mutex`
//! guard across `.await` (the `Caller<'_, Kernel>` is `!Send`).

use std::collections::HashMap;
use std::future::Future;
use std::net::IpAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};

use wasmtime::Caller;

use crate::kernel::Kernel;
use crate::mem;

// ─── constants ────────────────────────────────────────────────────────────

/// Project-private syscall number. See module docs.
pub const NR_RESOLVE: u32 = 400;

/// `struct addrinfo` size on wasm32-musl (4-byte pointers, no padding).
/// Verified: `ai_flags(i32) ai_family(i32) ai_socktype(i32)
/// ai_protocol(i32) ai_addrlen(i32) ai_addr(u32) ai_canonname(u32)
/// ai_next(u32)` = 8 × 4 = 32 bytes.
pub const ADDRINFO_SIZE: usize = 32;

/// `struct sockaddr_in` size — same on x86-64 and wasm32.
pub const SOCKADDR_IN_SIZE: usize = 16;

/// `struct sockaddr_in6` size — same on x86-64 and wasm32.
pub const SOCKADDR_IN6_SIZE: usize = 28;

/// Cap on the host→guest scratch region we write into. With a
/// reasonable IPv4-only result this fits comfortably; the limit
/// catches runaway result lists.
pub const MAX_RESOLVER_SCRATCH_BYTES: usize = 4096;

/// Cap on the per-process cache. Older entries are evicted by
/// `inserted` timestamp when we cross this — simple FIFO, not strict
/// LRU. 1024 names × ~64 bytes ≈ 64 KiB worst case.
pub const MAX_CACHE_ENTRIES: usize = 1024;

/// Where the host writes its result into guest memory, anchored to
/// `crate::kernel::MARKER_ADDR`. The conformance tests also use
/// MARKER_ADDR for `PASS`/`FAIL` markers — they're 64 bytes wide and
/// don't reach this offset.
pub const RESOLVER_SCRATCH_BASE: usize = 8192;

/// Default cache TTL (60 s). Override via `EDGE_RESOLVER_CACHE_TTL_MS`.
pub const DEFAULT_TTL_MS: u64 = 60_000;

/// Default per-lookup timeout (5 s). Override via `EDGE_RESOLVER_TIMEOUT_MS`.
pub const DEFAULT_TIMEOUT_MS: u64 = 5_000;

// ai_flags bit we reject in v1 (out of scope for now).
const AI_NUMERICHOST: i32 = 0x0004;
const AI_NUMERICSERV: i32 = 0x0400;

// ─── types ────────────────────────────────────────────────────────────────

/// TTL'd hostname → IP-list cache entry.
#[derive(Clone)]
pub struct CacheEntry {
    pub addrs: Vec<IpAddr>,
    pub inserted: Instant,
}

/// Resolver configuration that's plumbed in from the CLI env vars.
/// Mutated through `Kernel::attach_resolver_config`.
#[derive(Debug, Clone, Default)]
pub struct ResolverConfig {
    pub denylist: Vec<IpAddr>,
    pub ttl_ms: u64,
    pub timeout_ms: u64,
}

impl ResolverConfig {
    pub fn from_env(env: &[(String, String)]) -> Self {
        let mut cfg = Self {
            denylist: Vec::new(),
            ttl_ms: DEFAULT_TTL_MS,
            timeout_ms: DEFAULT_TIMEOUT_MS,
        };
        for (k, v) in env {
            match k.as_str() {
                "EDGE_RESOLVER_DENY" => {
                    for tok in v.split(',') {
                        let tok = tok.trim();
                        if tok.is_empty() {
                            continue;
                        }
                        match tok.parse::<IpAddr>() {
                            Ok(ip) => cfg.denylist.push(ip),
                            Err(_) => tracing::warn!(
                                target: "edge_cli::resolver",
                                "EDGE_RESOLVER_DENY: ignored invalid IP: {tok}"
                            ),
                        }
                    }
                }
                "EDGE_RESOLVER_CACHE_TTL_MS" => {
                    if let Ok(ms) = v.parse::<u64>() {
                        cfg.ttl_ms = ms;
                    }
                }
                "EDGE_RESOLVER_TIMEOUT_MS" => {
                    if let Ok(ms) = v.parse::<u64>() {
                        cfg.timeout_ms = ms;
                    }
                }
                _ => {}
            }
        }
        cfg
    }
}

/// Per-process resolver state — lives on `ProcessState` so threads in
/// the same process share the cache + denylist. Mirrors `futex_table`.
pub struct ResolverState {
    /// Lazy resolver backend. `None` until first NR_RESOLVE call.
    pub backend: Option<Arc<dyn ResolverBackend>>,
    /// TTL'd hostname → IP-list cache. Ejected lazily on lookup miss.
    pub cache: HashMap<String, CacheEntry>,
    /// Egress denylist. A lookup whose post-resolve IPs all intersect
    /// the denylist returns `-EAI_NONAME`.
    pub denylist: Vec<IpAddr>,
    /// Cache TTL in milliseconds.
    pub ttl_ms: u64,
    /// Per-lookup timeout in milliseconds.
    pub timeout_ms: u64,
}

impl Default for ResolverState {
    fn default() -> Self {
        Self {
            backend: None,
            cache: HashMap::new(),
            denylist: Vec::new(),
            ttl_ms: DEFAULT_TTL_MS,
            timeout_ms: DEFAULT_TIMEOUT_MS,
        }
    }
}

/// Async resolver backend. Production: `TokioResolverBackend`.
/// Tests: `StubResolver`.
///
/// Returns a boxed `Future` so the trait is dyn-compatible (RPFIT
/// — return-position `impl Trait` — can't be in trait method return
/// types if you also want to put the trait behind `dyn`). The
/// `Send + Sync` bounds keep `ProcessState` `Send + Sync` so it can
/// live behind an `Arc` shared across clone/fork threads.
pub trait ResolverBackend: Send + Sync {
    fn lookup_ip(
        &self,
        name: &str,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<Vec<IpAddr>>> + Send + '_>>;
}

// ─── marshal helper (pure fn, unit-testable) ──────────────────────────────

/// Build the byte payload that the host writes into guest memory.
///
/// Layout: for each IP, one `addrinfo` (32 bytes) followed by its
/// sockaddr (16 or 28 bytes). The linked list is laid out as a flat
/// array; `ai_next` carries the byte offset from
/// `RESOLVER_SCRATCH_BASE` to the next node (or 0 for the tail).
///
/// Returns `(bytes, head_offset_in_scratch)` — `head_offset_in_scratch`
/// is the byte offset from `RESOLVER_SCRATCH_BASE` to the first
/// `addrinfo`. The caller writes that u32 into the guest's res slot.
pub fn marshal_addrinfo_into_vec(
    addrs: &[IpAddr],
    ai_flags: i32,
    ai_socktype: i32,
    ai_protocol: i32,
    port: u16,
) -> (Vec<u8>, u32) {
    let n = addrs.len();
    if n == 0 {
        return (Vec::new(), 0);
    }

    // First pass: compute each node's start offset within the payload.
    let mut node_off: Vec<usize> = Vec::with_capacity(n);
    let mut sock_off: Vec<usize> = Vec::with_capacity(n);
    let mut cursor: usize = 0;
    for ip in addrs {
        node_off.push(cursor);
        cursor += ADDRINFO_SIZE;
        sock_off.push(cursor);
        cursor += if ip.is_ipv4() {
            SOCKADDR_IN_SIZE
        } else {
            SOCKADDR_IN6_SIZE
        };
    }

    let mut out = vec![0u8; cursor];

    // Second pass: write addrinfo headers, back-patching ai_next.
    for i in 0..n {
        let ip = addrs[i];
        let ai_family = if ip.is_ipv4() { 2i32 } else { 10i32 };
        let ai_addrlen = if ip.is_ipv4() {
            SOCKADDR_IN_SIZE
        } else {
            SOCKADDR_IN6_SIZE
        } as i32;
        let ai_next_guest_off = if i + 1 < n {
            (node_off[i + 1]) as u32
        } else {
            0
        };

        let off = node_off[i];
        out[off..off + 4].copy_from_slice(&ai_flags.to_le_bytes());
        out[off + 4..off + 8].copy_from_slice(&ai_family.to_le_bytes());
        out[off + 8..off + 12].copy_from_slice(&ai_socktype.to_le_bytes());
        out[off + 12..off + 16].copy_from_slice(&ai_protocol.to_le_bytes());
        out[off + 16..off + 20].copy_from_slice(&ai_addrlen.to_le_bytes());
        // ai_addr points into the same payload, offset from scratch base.
        out[off + 20..off + 24].copy_from_slice(&(sock_off[i] as u32).to_le_bytes());
        // ai_canonname = 0 in v1 (out of scope).
        out[off + 24..off + 28].copy_from_slice(&0u32.to_le_bytes());
        out[off + 28..off + 32].copy_from_slice(&ai_next_guest_off.to_le_bytes());
    }

    // Third pass: write sockaddr payloads.
    for (i, ip) in addrs.iter().enumerate() {
        let off = sock_off[i];
        match ip {
            IpAddr::V4(v4) => {
                let octets = v4.octets();
                out[off..off + 2].copy_from_slice(&2u16.to_le_bytes()); // AF_INET
                out[off + 2..off + 4].copy_from_slice(&port.to_be_bytes());
                out[off + 4..off + 8].copy_from_slice(&octets);
                // sin_zero (8 bytes) stays zero from `vec![0u8; cursor]`.
            }
            IpAddr::V6(v6) => {
                let octets = v6.octets();
                out[off..off + 2].copy_from_slice(&10u16.to_le_bytes()); // AF_INET6
                out[off + 2..off + 4].copy_from_slice(&port.to_be_bytes());
                out[off + 4..off + 8].copy_from_slice(&0u32.to_be_bytes()); // flowinfo
                out[off + 8..off + 24].copy_from_slice(&octets);
                out[off + 24..off + 28].copy_from_slice(&0u32.to_be_bytes()); // scope_id
            }
        }
    }

    let head_off = node_off[0] as u32;
    (out, head_off)
}

// ─── filter helper (pure fn) ──────────────────────────────────────────────

/// Apply family + denylist filters. `family` is one of 0 (UNSPEC),
/// 2 (AF_INET), 10 (AF_INET6). Returns the filtered list; if empty,
/// the caller should return `-EAI_NONAME`.
pub fn filter_addrs(addrs: &[IpAddr], family: i32, denylist: &[IpAddr]) -> Vec<IpAddr> {
    addrs
        .iter()
        .copied()
        .filter(|ip| match family {
            2 => ip.is_ipv4(),
            10 => ip.is_ipv6(),
            _ => true,
        })
        .filter(|ip| !denylist.contains(ip))
        .collect()
}

// ─── EAI constants (musl netdb.h — NEGATIVE) ─────────────────────────────

/// musl's `netdb.h` defines these as negative values. Returning
/// `-EAI_*` from the syscall matches musl's libc `getaddrinfo(3)`
/// contract exactly — the guest-side adapter casts the syscall
/// return to `int` and returns it directly.
pub const EAI_BADFLAGS: i64 = -1;
pub const EAI_NONAME: i64 = -2;
pub const EAI_AGAIN: i64 = -3;
pub const EAI_FAIL: i64 = -4;
pub const EAI_FAMILY: i64 = -6;
pub const EAI_SOCKTYPE: i64 = -7;
pub const EAI_SERVICE: i64 = -8;
pub const EAI_MEMORY: i64 = -10;
pub const EAI_SYSTEM: i64 = -11;
pub const EAI_OVERFLOW: i64 = -12;

// ─── handler ──────────────────────────────────────────────────────────────

/// `resolve(node_ptr, node_len, service_ptr, service_len, hints_ptr,
/// res_ptr_ptr)` — see module docs for the wire contract.
pub async fn resolve(caller: &mut Caller<'_, Kernel>, a: [i64; 6]) -> i64 {
    let node_ptr = a[0] as u32;
    let node_len = a[1];
    let service_ptr = a[2] as u32;
    let service_len = a[3];
    let hints_ptr = a[4] as u32;
    let res_ptr_ptr = a[5] as u32;

    // Touch the *_len args so the compiler doesn't warn; the length
    // cap is enforced inside `mem::guest_str` (256 for node, 64 for
    // service) and a 0 here means "scan to NUL."
    let _ = (node_len, service_len);

    // 1. Validate pointers — both NULL → fail.
    if node_ptr == 0 && service_ptr == 0 {
        return EAI_NONAME;
    }

    // 2. Read node (NUL-terminated, capped 256).
    let node = if node_ptr != 0 {
        match mem::guest_str(caller, node_ptr as i64, 256) {
            Ok(s) => s.to_string(),
            Err(_) => return EAI_SYSTEM,
        }
    } else {
        String::new()
    };

    // 3. Read + validate hints (if present).
    let mut ai_flags: i32 = 0;
    let mut ai_family: i32 = 0; // 0 = AF_UNSPEC
    let mut ai_socktype: i32 = 0;
    let mut ai_protocol: i32 = 0;
    if hints_ptr != 0 {
        let hints_bytes = match mem::guest_slice(caller, hints_ptr as i64, ADDRINFO_SIZE as i64) {
            Ok(b) => b,
            Err(_) => return EAI_SYSTEM,
        };
        ai_flags = i32::from_le_bytes(hints_bytes[0..4].try_into().unwrap());
        ai_family = i32::from_le_bytes(hints_bytes[4..8].try_into().unwrap());
        ai_socktype = i32::from_le_bytes(hints_bytes[8..12].try_into().unwrap());
        ai_protocol = i32::from_le_bytes(hints_bytes[12..16].try_into().unwrap());

        if ai_flags & (AI_NUMERICHOST | AI_NUMERICSERV) != 0 {
            return EAI_BADFLAGS;
        }
        match ai_family {
            0 | 2 | 10 => {}
            _ => return EAI_FAMILY,
        }
        match ai_socktype {
            0..=2 => {} // 0, SOCK_STREAM, SOCK_DGRAM
            _ => return EAI_SOCKTYPE,
        }
    }

    // 4. Parse service as a decimal u16.
    let port: u16 = if service_ptr != 0 {
        let svc = match mem::guest_str(caller, service_ptr as i64, 64) {
            Ok(s) => s,
            Err(_) => return EAI_SYSTEM,
        };
        match svc.parse::<u16>() {
            Ok(p) => p,
            Err(_) => return EAI_SERVICE,
        }
    } else {
        0
    };

    // Default socktype to SOCK_STREAM if not specified.
    let out_socktype = if ai_socktype == 0 { 1 } else { ai_socktype };

    // 5. Cache lookup (lock, clone, drop).
    let process_state = Arc::clone(&caller.data().process_state);
    {
        let state = process_state.resolver.lock();
        if let Some(entry) = state.cache.get(&node) {
            if state.ttl_ms == 0 || entry.inserted.elapsed() < Duration::from_millis(state.ttl_ms) {
                let cached = entry.addrs.clone();
                let denylist = state.denylist.clone();
                drop(state);
                return marshal_and_return(
                    caller,
                    &cached,
                    ai_flags,
                    ai_family,
                    out_socktype,
                    port,
                    ai_protocol,
                    &denylist,
                    res_ptr_ptr,
                );
            }
        }
    }

    // 6. Lazy backend init.
    let backend: Arc<dyn ResolverBackend> = {
        let mut state = process_state.resolver.lock();
        if state.backend.is_none() {
            state.backend = Some(Arc::new(TokioResolverBackend::new()));
        }
        Arc::clone(state.backend.as_ref().expect("just initialized"))
    };

    // 7. Lookup with timeout.
    let timeout_ms = process_state.resolver.lock().timeout_ms;
    let lookup =
        tokio::time::timeout(Duration::from_millis(timeout_ms), backend.lookup_ip(&node)).await;

    let addrs: Vec<IpAddr> = match lookup {
        Err(_) => return EAI_AGAIN,
        Ok(Err(_)) => return EAI_FAIL,
        Ok(Ok(v)) => v,
    };

    // 8. Cache insert (lock briefly).
    {
        let mut state = process_state.resolver.lock();
        if state.cache.len() >= MAX_CACHE_ENTRIES {
            // FIFO eviction: drop the oldest entry by `inserted`.
            if let Some(oldest_key) = state
                .cache
                .iter()
                .min_by_key(|(_, v)| v.inserted)
                .map(|(k, _)| k.clone())
            {
                state.cache.remove(&oldest_key);
            }
        }
        state.cache.insert(
            node.clone(),
            CacheEntry {
                addrs: addrs.clone(),
                inserted: Instant::now(),
            },
        );
    }

    // 9. Marshal + return.
    let denylist = process_state.resolver.lock().denylist.clone();
    marshal_and_return(
        caller,
        &addrs,
        ai_flags,
        ai_family,
        out_socktype,
        port,
        ai_protocol,
        &denylist,
        res_ptr_ptr,
    )
}

/// Filter + marshal + write back the head pointer. Returns the node
/// count or a negative EAI code on failure.
#[allow(clippy::too_many_arguments)]
fn marshal_and_return(
    caller: &mut Caller<'_, Kernel>,
    addrs: &[IpAddr],
    ai_flags: i32,
    ai_family: i32,
    ai_socktype: i32,
    port: u16,
    ai_protocol: i32,
    denylist: &[IpAddr],
    res_ptr_ptr: u32,
) -> i64 {
    let filtered = filter_addrs(addrs, ai_family, denylist);
    if filtered.is_empty() {
        return EAI_NONAME;
    }

    let (bytes, head_off) =
        marshal_addrinfo_into_vec(&filtered, ai_flags, ai_socktype, ai_protocol, port);
    if bytes.len() > MAX_RESOLVER_SCRATCH_BYTES {
        return EAI_MEMORY;
    }

    let scratch_base = RESOLVER_SCRATCH_BASE as i64;
    let buf = match mem::guest_slice_mut(caller, scratch_base, bytes.len() as i64) {
        Ok(b) => b,
        Err(_) => return EAI_SYSTEM,
    };
    buf.copy_from_slice(&bytes);

    // Write the head pointer (offset from scratch base) into the
    // guest's res slot. Addrinfo ai_addr entries inside the payload
    // already point to offsets from the same base.
    let out_slot = match mem::guest_slice_mut(caller, res_ptr_ptr as i64, 4) {
        Ok(b) => b,
        Err(_) => return EAI_SYSTEM,
    };
    out_slot.copy_from_slice(&head_off.to_le_bytes());

    filtered.len() as i64
}

// ─── production backend (hickory-resolver) ────────────────────────────────

/// Production resolver backend wrapping `hickory_resolver::TokioResolver`.
///
/// Lazily constructed on first NR_RESOLVE call. The constructor runs
/// synchronously and is fast (~10 ms cold on x86). The first lookup
/// may take longer (resolver config read + UDP socket bind), but
/// subsequent lookups reuse the same socket pool.
pub struct TokioResolverBackend {
    inner: hickory_resolver::TokioResolver,
}

impl TokioResolverBackend {
    pub fn new() -> Self {
        // hickory defaults to the host's `/etc/resolv.conf`. The
        // builder's `.expect()`s only fire on resource exhaustion
        // (OOM); the build's `.expect()` only fires if the host has
        // no resolvers at all and hickory can't synthesize one.
        let resolver = hickory_resolver::TokioResolver::builder_tokio()
            .expect("hickory builder")
            .build()
            .expect("hickory build: no resolvers configured");
        Self { inner: resolver }
    }
}

impl Default for TokioResolverBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl ResolverBackend for TokioResolverBackend {
    fn lookup_ip(
        &self,
        name: &str,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<Vec<IpAddr>>> + Send + '_>> {
        let resolver = self.inner.clone();
        let name = name.to_string();
        Box::pin(async move {
            let lookup = resolver.lookup_ip(name).await?;
            Ok(lookup.iter().collect())
        })
    }
}

// ─── test backend (deterministic) ────────────────────────────────────────

/// Test-only backend that returns a fixed IP list with optional
/// sleep. Used by `tests/resolve_conformance.rs` so we can run
/// integration tests offline (no network) and deterministically.
pub struct StubResolver {
    pub addrs: Vec<IpAddr>,
    pub sleep_ms: u64,
    /// Counters — incremented on each `lookup_ip` call. Tests assert
    /// that cache hits do NOT bump the counter.
    pub lookups: parking_lot::Mutex<u64>,
}

impl StubResolver {
    pub fn new(addrs: Vec<IpAddr>) -> Self {
        Self {
            addrs,
            sleep_ms: 0,
            lookups: parking_lot::Mutex::new(0),
        }
    }

    pub fn with_sleep(mut self, ms: u64) -> Self {
        self.sleep_ms = ms;
        self
    }

    pub fn lookup_count(&self) -> u64 {
        *self.lookups.lock()
    }
}

impl ResolverBackend for StubResolver {
    fn lookup_ip(
        &self,
        _name: &str,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<Vec<IpAddr>>> + Send + '_>> {
        let addrs = self.addrs.clone();
        let sleep = self.sleep_ms;
        *self.lookups.lock() += 1;
        Box::pin(async move {
            if sleep > 0 {
                tokio::time::sleep(Duration::from_millis(sleep)).await;
            }
            Ok(addrs)
        })
    }
}

// ─── unit tests ───────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, Ipv6Addr};

    /// `struct addrinfo` on wasm32-musl is exactly 32 bytes.
    /// Verified via the standard-layout rule: 8 × i32 / u32 fields, no
    /// padding required because all fields are 4-byte aligned.
    #[test]
    fn addrinfo_size_is_32() {
        assert_eq!(ADDRINFO_SIZE, 32);
        assert_eq!(std::mem::size_of::<[u32; 8]>(), 32);
    }

    #[test]
    fn sockaddr_in_size_is_16() {
        assert_eq!(SOCKADDR_IN_SIZE, 16);
    }

    #[test]
    fn sockaddr_in6_size_is_28() {
        assert_eq!(SOCKADDR_IN6_SIZE, 28);
    }

    #[test]
    fn marshal_single_v4_layout() {
        let addrs = vec![IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1))];
        let (bytes, head_off) = marshal_addrinfo_into_vec(&addrs, 0, 1, 0, 80);
        assert_eq!(bytes.len(), ADDRINFO_SIZE + SOCKADDR_IN_SIZE);
        assert_eq!(head_off, 0);

        // ai_flags (LE i32 = 0)
        assert_eq!(&bytes[0..4], &[0, 0, 0, 0]);
        // ai_family = 2 (AF_INET)
        assert_eq!(&bytes[4..8], &[2, 0, 0, 0]);
        // ai_socktype = 1 (SOCK_STREAM)
        assert_eq!(&bytes[8..12], &[1, 0, 0, 0]);
        // ai_protocol = 0
        assert_eq!(&bytes[12..16], &[0, 0, 0, 0]);
        // ai_addrlen = 16
        assert_eq!(&bytes[16..20], &[16, 0, 0, 0]);
        // ai_addr = scratch offset of sockaddr (32)
        assert_eq!(&bytes[20..24], &[32, 0, 0, 0]);
        // ai_canonname = 0
        assert_eq!(&bytes[24..28], &[0, 0, 0, 0]);
        // ai_next = 0 (single node)
        assert_eq!(&bytes[28..32], &[0, 0, 0, 0]);

        // sockaddr_in starts at offset 32:
        // sin_family = 2 LE u16
        assert_eq!(&bytes[32..34], &[2, 0]);
        // sin_port = 80 BE u16
        assert_eq!(&bytes[34..36], &[0, 80]);
        // sin_addr = 127.0.0.1
        assert_eq!(&bytes[36..40], &[127, 0, 0, 1]);
        // sin_zero = 8 zeros
        assert_eq!(&bytes[40..48], &[0; 8]);
    }

    #[test]
    fn marshal_single_v6_sin6_addr_is_network_order() {
        let addrs = vec![IpAddr::V6(Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 1))];
        let (bytes, _head) = marshal_addrinfo_into_vec(&addrs, 0, 1, 0, 443);
        // ai_family = 10 (AF_INET6)
        assert_eq!(&bytes[4..8], &[10, 0, 0, 0]);
        // ai_addrlen = 28
        assert_eq!(&bytes[16..20], &[28, 0, 0, 0]);
        // ai_addr at offset 32 (node) + 0 = 32
        assert_eq!(&bytes[20..24], &[32, 0, 0, 0]);
        // sockaddr_in6 starts at 32:
        // sin6_family = 10 LE u16
        assert_eq!(&bytes[32..34], &[10, 0]);
        // sin6_port = 443 BE u16
        assert_eq!(&bytes[34..36], &[1, 187]); // 443 = 0x01BB
                                               // sin6_flowinfo = 0 BE
        assert_eq!(&bytes[36..40], &[0, 0, 0, 0]);
        // sin6_addr = 2001:0db8:0000:0000:0000:0000:0000:0001 (network order)
        assert_eq!(
            &bytes[40..56],
            &[0x20, 0x01, 0x0d, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1]
        );
        // sin6_scope_id = 0
        assert_eq!(&bytes[56..60], &[0, 0, 0, 0]);
    }

    #[test]
    fn marshal_port_8080_is_be() {
        let addrs = vec![IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0))];
        let (bytes, _) = marshal_addrinfo_into_vec(&addrs, 0, 1, 0, 8080);
        // 8080 = 0x1F90 BE
        assert_eq!(&bytes[34..36], &[0x1F, 0x90]);
    }

    #[test]
    fn marshal_three_addrs_chain_ai_next_correctly() {
        let addrs = vec![
            IpAddr::V4(Ipv4Addr::new(1, 2, 3, 4)),
            IpAddr::V4(Ipv4Addr::new(5, 6, 7, 8)),
            IpAddr::V6(Ipv6Addr::LOCALHOST),
        ];
        let (bytes, head_off) = marshal_addrinfo_into_vec(&addrs, 0, 1, 0, 0);
        assert_eq!(head_off, 0);

        // Node 0: ai_next -> offset of node 1 = 32 (addr) + 16 (sock) = 48
        assert_eq!(&bytes[28..32], &[48, 0, 0, 0]);
        // Node 1: ai_next -> offset of node 2 = 48 + 32 + 16 = 96
        assert_eq!(&bytes[48 + 28..48 + 32], &[96, 0, 0, 0]);
        // Node 2: ai_next -> 0 (tail)
        assert_eq!(&bytes[96 + 28..96 + 32], &[0, 0, 0, 0]);

        // Total: 3 * 32 + 2 * 16 + 1 * 28 = 96 + 32 + 28 = 156
        assert_eq!(bytes.len(), 156);
    }

    #[test]
    fn marshal_empty_addrs_returns_empty_bytes() {
        let (bytes, head) = marshal_addrinfo_into_vec(&[], 0, 1, 0, 0);
        assert!(bytes.is_empty());
        assert_eq!(head, 0);
    }

    #[test]
    fn filter_drops_denied_keeps_others() {
        let addrs = vec![
            IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)),
            IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)),
        ];
        let denylist = vec![IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1))];
        let out = filter_addrs(&addrs, 0, &denylist);
        assert_eq!(out, vec![IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1))]);
    }

    #[test]
    fn filter_drops_all_returns_empty() {
        let addrs = vec![IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1))];
        let denylist = vec![IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1))];
        let out = filter_addrs(&addrs, 0, &denylist);
        assert!(out.is_empty());
    }

    #[test]
    fn filter_family_v4_only() {
        let addrs = vec![
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            IpAddr::V6(Ipv6Addr::LOCALHOST),
        ];
        let out = filter_addrs(&addrs, 2, &[]);
        assert_eq!(out, vec![IpAddr::V4(Ipv4Addr::LOCALHOST)]);
    }

    #[test]
    fn filter_family_v6_only() {
        let addrs = vec![
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            IpAddr::V6(Ipv6Addr::LOCALHOST),
        ];
        let out = filter_addrs(&addrs, 10, &[]);
        assert_eq!(out, vec![IpAddr::V6(Ipv6Addr::LOCALHOST)]);
    }

    #[test]
    fn filter_family_unspec_keeps_all() {
        let addrs = vec![
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            IpAddr::V6(Ipv6Addr::LOCALHOST),
        ];
        let out = filter_addrs(&addrs, 0, &[]);
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn from_env_defaults_when_empty() {
        let cfg = ResolverConfig::from_env(&[]);
        assert!(cfg.denylist.is_empty());
        assert_eq!(cfg.ttl_ms, DEFAULT_TTL_MS);
        assert_eq!(cfg.timeout_ms, DEFAULT_TIMEOUT_MS);
    }

    #[test]
    fn from_env_parses_denylist() {
        let env = vec![(
            "EDGE_RESOLVER_DENY".to_string(),
            "127.0.0.1, ::1, 10.0.0.5".to_string(),
        )];
        let cfg = ResolverConfig::from_env(&env);
        assert_eq!(cfg.denylist.len(), 3);
        assert!(cfg
            .denylist
            .contains(&IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1))));
        assert!(cfg.denylist.contains(&IpAddr::V6(Ipv6Addr::LOCALHOST)));
    }

    #[test]
    fn from_env_ignores_malformed_ip() {
        let env = vec![(
            "EDGE_RESOLVER_DENY".to_string(),
            "not-an-ip, 127.0.0.1, also-bad".to_string(),
        )];
        let cfg = ResolverConfig::from_env(&env);
        assert_eq!(cfg.denylist.len(), 1);
        assert!(cfg
            .denylist
            .contains(&IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1))));
    }

    #[test]
    fn from_env_parses_ttl_and_timeout() {
        let env = vec![
            (
                "EDGE_RESOLVER_CACHE_TTL_MS".to_string(),
                "30000".to_string(),
            ),
            ("EDGE_RESOLVER_TIMEOUT_MS".to_string(), "2000".to_string()),
        ];
        let cfg = ResolverConfig::from_env(&env);
        assert_eq!(cfg.ttl_ms, 30_000);
        assert_eq!(cfg.timeout_ms, 2_000);
    }

    #[test]
    fn nr_resolve_is_400() {
        assert_eq!(NR_RESOLVE, 400);
    }
}
