# ADR 0007 — P2-DNS: project-private NR_RESOLVE syscall

## Status

Accepted (2026-07-15). Stub — will be finalized in the docs commit.

## Context

DNS resolution is needed for FastAPI/httpx/SQLAlchemy workloads. musl's
UDP-based `getaddrinfo` path is unusable in this kernel:

1. NR 63 is already `uname` ([`src/sys/identity.rs:19`](../../src/sys/identity.rs:19));
   there is no Linux `getaddrinfo` syscall at any NR.
2. No UDP socket layer exists in the kernel (`SocketKind::Datagram`
   exists as an enum tag, but `sendto`/`recvfrom` are TCP-only).
3. wasm32-musl `EAI_*` are negative (`EAI_NONAME = -2`); wasm32
   `struct addrinfo` is 32 bytes (4-byte pointers).

Three paths were considered:

- **A. Add UDP socket layer + let musl resolve.** Months of work;
  outside the P2 scope.
- **B. Project-private NR + guest libc adapter.** Chosen.
- **C. Re-implement getaddrinfo on the guest side using existing
  syscalls.** Guest is `--disable-threads --without-threads
  --disable-ipv6 --disable-ssl` ([`guest/build.sh:72-79`](../../guest/build.sh:72)).
  No asyncio executor, no IPv6, no TLS.

## Decision

Path B:

- **`NR_RESOLVE = 400`** in the upstream-reserved range 387-423
  (per `arch/x86/entry/syscalls/syscall_64.tbl` header). Inside that
  range is guaranteed safe.
- **Per-`ProcessState` placement**: mirrors `futex_table` (ADR 0006),
  shared across `clone`/`fork` via `Arc<ProcessState>`.
- **`ResolverBackend` trait**: production = `TokioResolverBackend`
  (wraps `hickory_resolver::TokioResolver`); test = `StubResolver`.
- **EAI sign convention**: negative (matches musl). No translation.
- **Snapshot non-persistence**: rebuild on restore. Denylist is
  operator-supplied via env vars on `serve`.
- **Guest-side adapter**: `guest/resolver/{getaddrinfo,freeaddrinfo}.c`
  override musl's weak symbols via link-order.

## Consequences

- New module `src/sys/resolver.rs` (handler + helpers + tests).
- New `guest/resolver/` directory (two `.c` files + one `.h`).
- New `tests/resolve_conformance.rs` (6 cases).
- New `tests/conformance/{getaddrinfo_loopback,getaddrinfo_eai_noname}.c`.
- New dep: `hickory-resolver = { version = "0.25",
  default-features = false, features = ["tokio", "system-config"] }`.

## Deferred (out of scope for v1)

- UDP socket layer in kernel (Path A).
- `AI_NUMERICHOST` / `AI_NUMERICSERV` hint flags → `-EAI_BADFLAGS`.
- `getservbyname()` → numeric-only service strings.
- PTR (reverse DNS).
- Per-record denylist (currently IP-level post-filter).
- Snapshot persistence of denylist config.

Final ADR content lands in the docs commit alongside HANDOFF regen.