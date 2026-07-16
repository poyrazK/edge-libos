# ADR 0008 — Path A: UDP socket layer (musl-native DNS)

## Status

Accepted (2026-07-16). Stub — to be finalized in C8 (the docs commit).
Implementation branch `worktree-p3-udp-socket-layer`; commits land C0→C8
per the plan at `/Users/poyrazk/.claude/plans/lets-create-imp-plan-wise-clarke.md`.

## Context

ADR 0007 (P2-DNS, shipped PR #27) delivered DNS resolution by adding a
project-private `NR_RESOLVE=400` syscall that carries the semantics of
`getaddrinfo(3)` over a host `hickory-resolver` instance. That decision
explicitly deferred Path A:

> UDP socket layer in kernel (Path A) — separate, larger workstream.

Path A makes musl's own `getaddrinfo(3)` work end-to-end by implementing
real UDP support (`tokio::net::UdpSocket` + `sendto`/`recvfrom`/`sendmsg`/
`recvmsg`/`connect` + virtual `/etc/hosts` and `/etc/resolv.conf`). It
also unblocks every other UDP workload (QUIC, mDNS, custom protocols).

## Decision

- **Keep `NR_RESOLVE` as fast path.** Add `EDGE_RESOLVE_BACKEND`
  env var with values `hickory` (default), `musl`, or `auto` (`auto` =
  hickory cache hit wins, miss falls to musl UDP).
- **`UdpSocketState`** in a new `src/sys/udp.rs`:
  `Arc<tokio::net::UdpSocket>` + `Arc<Mutex<VecDeque<Datagram>>>`
  + `Arc<Notify>` (read/write) + `bound_addr` + `peer_addr` + `family`
  + `shutdown_flags` + `pump_handle: Option<JoinHandle>`
  + `pump_cancel: Arc<AtomicBool>`.
- **Background pump task per UDP socket** — owns the host
  `Arc<UdpSocket>`; loops `udp.readable().await`; drains into the
  bounded `VecDeque<Datagram>`; fires `notify_read.notify_waiters()` on
  packet arrival. poll/epoll wake via existing `Arc<Notify>` machinery.
- **Virtual `/etc/hosts` and `/etc/resolv.conf`** — synthesized in
  `src/sys/path.rs::resolve_via_cwd_or_root`. No host-FS passthrough;
  no operator symlink step. `EDGE_DNS_SERVERS` env var populates the
  synthesized resolv.conf; default loopback-only `/etc/hosts`.
- **`socket2` direct dep** (currently transitive) — used to set
  `IPV6_V6ONLY` + `SO_REUSEADDR` before `UdpSocket::bind`.
- **Snapshot** — `SocketSnapshot` gains additive `udp_bound_v4/v6`,
  `udp_peer_v4/v6`, `ipv6_v6only` fields (serde-default per
  ADR 0005 precedent). Apply path branches on `sock_kind == Datagram`
  before the existing `!is_acceptor` rejection. Rebuild
  `UdpSocket::bind` on apply; **in-flight datagrams + pump task +
  Notify waiters are dropped** (documented).
- **mDNS** — explicitly **deferred follow-up**. Requires
  `SO_BROADCAST` + `IP_MULTICAST_{IF,TTL,LOOP,ADD_MEMBERSHIP}` +
  `IPV6_JOIN_GROUP`; out of scope for this milestone.

## Consequences

(Stub — populated in C8 once all 8 commits land.)

## Deferred (out of scope for v1)

- mDNS / multicast memberships.
- ICMP error → `SO_ERROR` relay on connected UDP.
- `SO_TIMESTAMP` cmsg population on `recvmsg`.
- `UDP_CORK` / `UDP_ENCAP` / `UDP_GRO`.
- Snapshot in-flight datagram queue preservation.

Final ADR content lands in the docs commit (C8) alongside HANDOFF regen.