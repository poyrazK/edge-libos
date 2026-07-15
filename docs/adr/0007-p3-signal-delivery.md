# ADR 0007 — P3 signal delivery (EINTR + default actions)

## Status

Proposed 2026-07-15. Realized by the signal-delivery work on branch
`p3-v2-signal-delivery`. Implementation is staged across commits:
C1 (this ADR + `deliverable()` helper + `ProcessState`/`Kernel` fields),
C2 (kill/tgkill wake firing + dispatch pre-check), C3–C7 (per-syscall
`select!` signal arms: epoll, poll/select, futex, bare-await, wait4),
C8 (terminating default action wiring), C9 (`SIGUSR1` → freeze
quiescence), C10 (docs + integration sweep).

## Context

Since P0 the kernel has *recorded* signal dispositions
(`rt_sigaction`/`rt_sigprocmask`/`sigaltstack`) and, since P3 Tier-8 v2
(ADR 0006 §6), enqueued `kill`/`tgkill` signals into
`ProcessState.signals_pending` — but nothing ever *delivered* them. The
file docstring of `src/sys/signal.rs` said so verbatim ("no real
delivery — spec §4.8"), `EINTR` (`errno.rs`) was defined but returned by
no handler, and `signals_pending` had producers but no consumer.

This blocked three things called out across the codebase:

1. **Signal-aware `wait4`** — `WUNTRACED`/`WCONTINUED`/`WNOWAIT`/`WALL`
   were rejected with `-EINVAL` "until a real signal delivery story
   lands" (HANDOFF.md).
2. **Host-driven freeze quiescence** — the `SIGUSR1 → SIGSNAPSHOT`
   follow-up (ADR 0004 §5) needs a way to interrupt a guest parked in
   `epoll_wait`/`accept4` so `edge-cli freeze` can snapshot a guest that
   does not call `NR_SNAPSHOT` itself.
3. Any guest expecting a blocked syscall to return `-EINTR`.

Spec §4.8 scopes v1 to **synthetic signals only** — "No async POSIX
signal delivery into the guest [handlers]." This ADR delivers exactly
that scope and no more.

## Decision

### §1. Scope — EINTR + default actions, no handler invocation

Signal delivery in v1 means:

- An **unmasked** pending signal interrupts a *blocking* syscall,
  which returns `-EINTR`.
- A **default-terminating** signal tears the guest down with exit code
  `128 + signo` (shell convention).
- **Ignored** signals (`SIG_IGN`, or a default-ignore signal under
  `SIG_DFL`) are consumed and discarded.

We explicitly do **NOT** synthesize a call into a guest-registered
`sa_handler`: no signal-frame construction, no `sigaltstack` switch, no
`rt_sigreturn` context restore. A signal with a custom handler is
consumed and downgraded to an `-EINTR` interrupt (the guest's syscall
loop sees `EINTR` but its handler never runs). This is a documented
partial-delivery contract; full handler invocation is future work.

### §2. `deliverable()` — the delivery decision

`sys::signal::deliverable(&Kernel) -> DeliveryAction` where
`DeliveryAction = Ignore | Interrupt | Terminate(i32)`. It drains
`process_state.signals_pending` under the `parking_lot::Mutex` (fully
released before return — never held across `.await`, per ADR 0001 §2)
and, per dequeued signal:

- sig `0` → drop (it is a `kill(pid, 0)` permission probe, not a
  real signal);
- `SIGKILL`(9) / `SIGSTOP`(19) → `Terminate` immediately, bypassing
  both the mask and any recorded disposition (uncatchable);
- masked (bit `signo-1` set in `signals.mask`) → left on the queue,
  preserving FIFO order, so a later `rt_sigprocmask` unblock delivers
  it;
- `SIG_IGN`, or `SIG_DFL` with a default-ignore disposition
  (SIGCHLD/SIGCONT/SIGURG/SIGWINCH) → drop;
- `SIG_DFL` with a default-terminate disposition → `Terminate(signo)`;
- custom handler → `Interrupt` (consumed, handler not run).

`SIGSTOP` is treated as `Terminate(19)` in v1 (exit 147) because there
is no job-control pause; documented as a v1 simplification.

### §3. Per-tid wake primitive

`ProcessState.signal_wakes: Mutex<HashMap<i32, Arc<Notify>>>`
(lazy-created per tid). A thread parking in a blocking syscall clones
its tid's `Arc<Notify>` out and adds it as a `select!` arm.
`kill`/`tgkill`, after pushing onto `signals_pending`, look up (or
create) the target tid's `Notify`, drop the lock, then
`notify_waiters()` — mirroring the `reap_all_children`
clone-then-drop-then-notify pattern. Per-process scope is required: the
signal *sender* runs on a different fiber than the target and cannot
reach the target's (`!Send`) `Kernel`. Runtime-only; never serialized.

### §4. Terminating default action — cooperative, no trap

`exit`/`exit_group` already set `kernel.exit_code` and return `0`
(never trap), and the run path surfaces `exit_code.unwrap_or(0)` after
`_start` returns. Signal termination reuses this: `deliverable()`'s
`Terminate(signo)` handler sets `kernel.exit_code = Some(128 + signo)`
and `kernel.exit_requested = true` (an `AtomicBool`). `dispatch()`
checks `exit_requested` at the top and short-circuits every subsequent
syscall to `0`, so the guest's libc unwinds and the run path reports
the code. `exit_requested` is set *only* by signal delivery — an
explicit `exit(0)` stays `false`.

### §5. Blocking-syscall integration

Each blocking point gains a signal-wake `select!` arm calling
`deliverable()`: `Interrupt` → `-EINTR`; `Terminate` → set exit + return
`-EINTR`; `Ignore` (spurious/lost-wake) → re-park (wait4-style loop).
Covered call sites: `epoll_wait`/`epoll_pwait`, `poll`/`ppoll`/`select`,
`futex(FUTEX_WAIT)`, `nanosleep`/`clock_nanosleep`, `accept4`,
`recvfrom`/`recvmsg`, `wait4` (both specific-pid and any-pid arms).

### §6. SIGUSR1 → freeze quiescence

Kept orthogonal to real delivery. `ProcessState.quiesce_notify:
Option<Arc<Notify>>` is `None` for a normal `run`; `edge-cli freeze`
installs an `Arc<Notify>` and fires it from a
`tokio::signal::unix::signal(SignalKind::user_defined1())` listener.
Blocking syscalls race `quiesce_notify` as an extra `select!` arm *only*
when `Some`, and on that wake continue normally (NOT `-EINTR`) — the
guest is left at a well-defined in-syscall quiescent point. `freeze`'s
outer `timeout(10s, call_start)` becomes a `select!` over `call_start` /
`quiesce_notify` / 10s. The `SIGUSR1` listener touches only the
`Send + Sync` `Arc<Notify>`, never the `!Send` `Store`.

### §7. Snapshot policy

Pending signals are **not** serialized. They are transient and
non-deterministic; replaying one on `serve` would re-deliver a signal
whose source no longer exists, violating the deterministic-replay
contract (ADR 0002/0004). `signal_wakes` and `quiesce_notify` are
runtime handles (like `child_event`) and are never serialized. No
snapshot format-version change. Contract: **pending signals are dropped
across freeze/serve.**

## Consequences

- Guests that loop on `-EINTR` (uvicorn's signal-wakeup self-pipe,
  CPython's `signal` module checks) now see interrupts instead of
  hanging forever.
- `wait4` can be interrupted, unblocking the signal-aware `wait4`
  follow-up.
- Operators can `kill -USR1` an `edge-cli freeze` to snapshot a
  parked server guest.
- Guests relying on their own `sa_handler` running still do not get it
  — tracked as future work (handler-frame synthesis).
