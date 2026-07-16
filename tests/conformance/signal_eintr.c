//! ADR 0007 §1 / §5: signal delivery wiring end-to-end via `kill`.
//!
//! v1 scope is `-EINTR` + default actions, not handler invocation
//! (ADR 0007 §1). The actual `-EINTR` interrupt path is exercised
//! by the Rust integration tests in `tests/signal_conformance.rs`
//! (which can pre-arm the signal queue from a second fiber);
//! this C conformance fixture verifies the integration-level
//! contract:
//!
//!   - `rt_sigaction(SIGUSR1, custom-handler)` records a
//!     disposition (returns 0).
//!   - `kill(getpid(), SIGUSR1)` queues a signal into
//!     `signals_pending` and (via C2) fires the target tid's
//!     `signal_wakes` notify.
//!   - `kill(getpid(), 0)` permission probe succeeds (returns 0).
//!
//! Per ADR 0007 §2, a custom-handler signal is consumed and
//! downgraded to `-EINTR` — the handler is NOT invoked. That
//! partial-delivery contract is covered by Rust tests.
//!
//! Marker convention: write "PASS\n" to memory[4096] on success,
//! "FAIL:<reason>\n" on failure. The runner reads the marker back.

#include "syscall.h"

__attribute__((visibility("default")))
void _start(void) {
    // Step 1: install a SIGUSR1 handler (any non-zero, non-one
    // address counts as "custom handler" — `deliverable()` only
    // checks the disposition slot, not the validity of the
    // address, since we never invoke it in v1).
    struct {
        int64_t handler;
        int64_t flags;
        int64_t mask;
        int64_t restorer;
    } act;
    act.handler = 0x1000; /* any non-zero address */
    act.flags = 0;
    act.mask = 0;
    act.restorer = 0;
    int64_t rc = sc4(NR_RT_SIGACTION, 10 /*SIGUSR1*/, (int64_t)(intptr_t)&act, 0, 8);
    if (rc != 0) { mark_fail("rt_sigaction install failed"); return; }

    // Step 2: self-signal SIGUSR1. This enqueues into
    // `signals_pending` and fires `signal_wakes[tid].notify_waiters()`
    // per ADR 0007 §3 — observable as a wake on any `select!` arm
    // that races the per-tid notify (covered by Rust tests).
    int64_t pid = sc1(NR_GETPID, 0);
    if (pid <= 0) { mark_fail("getpid failed"); return; }
    rc = sc2(NR_KILL, pid, 10 /*SIGUSR1*/);
    if (rc != 0) { mark_fail("kill returned non-zero"); return; }

    // Step 3: permission probe via kill(pid, 0) — should succeed
    // regardless of pending signals.
    rc = sc2(NR_KILL, pid, 0);
    if (rc != 0) { mark_fail("kill permission probe failed"); return; }

    mark_pass();
}
