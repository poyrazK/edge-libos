//! ADR 0007 §4: a default-terminating signal stops the guest with
//! exit code `128 + signo` (shell convention).
//!
//! `SIGTERM` (15) is default-terminate. After `kill(self, SIGTERM)`,
//! the next blocking syscall returns `-EINTR` (per ADR 0007 §5),
//! AND `kernel.exit_code` is set to `128 + 15 = 143`. We can't
//! observe `exit_code` from inside `_start` (the terminate path
//! just sets it; the run path surfaces it after `_start` returns).
//! So this C fixture verifies the contract via the next-syscall
//! behavior:
//!
//!   - `kill(self, SIGTERM)` queues the signal.
//!   - `nanosleep(2s)` is blocking. With the C6 signal arm in
//!     place, it returns `-EINTR` (= `-4`) immediately because
//!     SIGTERM is deliverable.
//!   - The signal is consumed by `deliverable()` via the
//!     blocking-syscall select! arm — and `apply_terminate_if_needed`
//!     runs (idempotent: sets `exit_code = 143` and
//!     `exit_requested = true`).
//!
//! Marker convention: write "PASS\n" to memory[4096] on success,
//! "FAIL:<reason>\n" on failure.

#include "syscall.h"

__attribute__((visibility("default")))
void _start(void) {
    int64_t pid = sc1(NR_GETPID, 0);
    if (pid <= 0) { mark_fail("getpid failed"); return; }

    // Queue SIGTERM (15) on self.
    int64_t rc = sc2(NR_KILL, pid, 15 /*SIGTERM*/);
    if (rc != 0) { mark_fail("kill returned non-zero"); return; }

    // Block on nanosleep(2s). The signal arm should fire and
    // return -EINTR immediately. SIGTERM is default-terminate,
    // which `apply_terminate_if_needed` converts to exit_code=143
    // + exit_requested=true; the syscall returns -EINTR per
    // ADR 0007 §5 (Terminate short-circuits to -EINTR via the
    // same code path).
    int64_t req[2] = {2, 0}; /* 2 seconds, 0 ns */
    int64_t rem[2] = {0, 0};
    int64_t ret = sc4(NR_NANOSLEEP, (int64_t)(intptr_t)req, (int64_t)(intptr_t)rem, 0, 0);
    if (ret != -4 /*-EINTR*/) {
        mark_fail("nanosleep did not return -EINTR after SIGTERM");
        return;
    }

    mark_pass();
}
