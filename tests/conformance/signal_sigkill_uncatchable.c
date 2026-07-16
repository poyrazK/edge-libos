//! ADR 0007 §2: SIGKILL (9) and SIGSTOP (19) are uncatchable.
//!
//! Per the Linux ABI, these two signals bypass the disposition
//! entirely — even if the guest has installed a custom handler
//! or SIG_IGN'd them, `deliverable()` must `Terminate(signo)`
//! regardless. We prove this by:
//!
//!   - `rt_sigaction(SIGKILL, SIG_IGN)` — installs SIG_IGN.
//!     This normally records `disposition[SIGKILL] = SIG_IGN`,
//!     but `deliverable()` ignores the disposition for SIGKILL
//!     and SIGSTOP per the §2 rule.
//!   - `kill(self, SIGKILL)` — queues the signal.
//!   - Next syscall short-circuits to 0 (pre-check fires).
//!     If `deliverable()` had respected SIG_IGN, the syscall
//!     would have returned a real value.
//!
//! Marker convention: write "PASS\n" to memory[4096] on success,
//! "FAIL:<reason>\n" on failure.

#include "syscall.h"

// Standard POSIX SIG_IGN/SIG_DFL values from include/linux/signal.h.
#define SIG_DFL 0
#define SIG_IGN 1

__attribute__((visibility("default")))
void _start(void) {
    // Try to ignore SIGKILL. The kernel records the disposition
    // (rt_sigaction returns 0), but `deliverable()` bypasses it
    // because SIGKILL is uncatchable.
    struct {
        int64_t handler;
        int64_t flags;
        int64_t mask;
        int64_t restorer;
    } act;
    act.handler = SIG_IGN;
    act.flags = 0;
    act.mask = 0;
    act.restorer = 0;
    int64_t rc = sc4(NR_RT_SIGACTION, 9 /*SIGKILL*/, (int64_t)(intptr_t)&act, 0, 8);
    if (rc != 0) { mark_fail("rt_sigaction SIG_IGN SIGKILL failed"); return; }

    // Queue SIGKILL on self.
    int64_t pid = sc1(NR_GETPID, 0);
    if (pid <= 0) { mark_fail("getpid failed"); return; }
    rc = sc2(NR_KILL, pid, 9 /*SIGKILL*/);
    if (rc != 0) { mark_fail("kill returned non-zero"); return; }

    // SIG_IGN was bypassed: pre-check fires, syscall returns 0.
    int64_t after = sc1(NR_GETPID, 0);
    if (after != 0) {
        mark_fail("SIGKILL was catchable (SIG_IGN honored) — disposition bypass broken");
        return;
    }

    mark_pass();
}
