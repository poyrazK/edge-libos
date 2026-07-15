# Architecture Decision Records

This directory holds the ADRs for `edge-libos`. Each ADR pins a
concrete decision before the implementer lands the code, so the
implementation has a contract to honor.

## Index

- [ADR 0001 — P3 futex semantics](0001-p3-futex-semantics.md) — Accepted.
  Pinned the `futex(2)` integration contract (u32 guest addresses,
  per-PID `Notify` scheme, snapshot allowlist format).
- [ADR 0002 — snapshot wire format](0002-snapshot-wire-format.md) — Accepted.
  Pinned the `postcard` container, explicit `LeU32` / `LeU64` newtypes,
  sparse per-page memory layout, format-version rule.
- [ADR 0003 — P3 live migration](0003-p3-live-migration.md) — Accepted.
  Pinned the v1 freeze-then-serve migration flow: module portability,
  drain semantics, format-version interaction, accepted-stream +
  abstract-namespace rejection, and the `Subcommand::Migrate` wrapper.
- [ADR 0004 — metering semantics](0004-metering-semantics.md) — Accepted.
  Pinned fuel/notify-yield interaction, `cpu_ns` semantics, cold-start
  bench thresholds (`p50 < 5 ms`), and the bench output format.
- [ADR 0005 — P3 clone threads (v2 of fork/clone/wait4)](0005-p3-clone-threads.md) — Accepted.
  Pinned the per-thread Kernel field split (`Arc<ProcessState>`),
  `Arc<Engine>` + `Arc<Module>` ownership, `Arc<Notify>` clone-on-lock-out
  multi-waiter pattern (replacing v1's single-waiter `Option<Waker>`),
  the v2-supported clone flag set (`CLONE_VM | CLONE_THREAD |
  CLONE_CHILD_SETTID | CLONE_PARENT_SETTID`), tgid/tid routing for
  `kill` / `tgkill`, and the child-thread panic sentinel (139).