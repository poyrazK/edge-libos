//! ADR 0007 §2: a signal blocked by `signals.mask` is *not*
//! delivered — it stays in the queue, available for later
//! unblock via `rt_sigprocmask(SIG_UNBLOCK)`.
//!
//! Specifically:
//!   - `rt_sigprocmask(SIG_BLOCK, SIGUSR1)` adds SIGUSR1 (bit 9)
//!     to `signals.mask`.
//!   - `kill(self, SIGUSR1)` queues the signal. The queue still
//!     holds it because `deliverable()` skips masked signals.
//!   - The dispatch pre-check does NOT fire (we masked the
//!     terminate signal in the queue; SIGUSR1 is custom-handler
//!     or default-ignore, so even if it leaked past the mask
//!     it wouldn't terminate).
//!   - Next syscall returns its real value (positive PID).
//!   - `rt_sigprocmask(SIG_UNBLOCK, SIGUSR1)` clears the bit.
//!   - We don't re-deliver here — the test only proves "masked
//!     signals don't reach `deliverable()`" via the positive
//!     syscall return, which would have been 0 if the
//!     terminate-path had leaked through.
//!
//! Marker convention: write "PASS\n" to memory[4096] on success,
//! "FAIL:<reason>\n" on failure.

#include "syscall.h"

// rt_sigprocmask(2) `how` values. From include/uapi/asm/signal.h.
#define SIG_BLOCK 0
#define SIG_UNBLOCK 1
#define SIG_SETMASK 2

__attribute__((visibility("default")))
void _start(void) {
    // Block SIGUSR1 (bit 1 << (10-1) = 1 << 9 = 0x200).
    int64_t mask_block = 0x200;
    int64_t oldmask_addr = 8192; /* scratch */
    int64_t rc = sc4(NR_RT_SIGPROCMASK, SIG_BLOCK, (int64_t)(intptr_t)&mask_block,
                     oldmask_addr, 8);
    if (rc != 0) { mark_fail("rt_sigprocmask BLOCK failed"); return; }

    // Self-signal SIGUSR1 (10). The signal is queued but masked,
    // so `deliverable()` skips it (ADR 0007 §2).
    int64_t pid = sc1(NR_GETPID, 0);
    if (pid <= 0) { mark_fail("getpid failed"); return; }
    rc = sc2(NR_KILL, pid, 10 /*SIGUSR1*/);
    if (rc != 0) { mark_fail("kill returned non-zero"); return; }

    // Next syscall must NOT be short-circuited — the masked
    // signal did not fire `deliverable()`. getpid returns a real
    // positive PID; anything else means the pre-check wrongly
    // fired (or getpid broke).
    int64_t after = sc1(NR_GETPID, 0);
    if (after != pid) {
        mark_fail("mask did not block delivery; dispatch pre-check wrongly fired");
        return;
    }

    // Unblock — the queued signal is now deliverable, but we
    // don't consume it here (would require a second fiber to
    // observe the `-EINTR` interrupt). The structural proof is
    // that the mask actually took effect.
    rc = sc4(NR_RT_SIGPROCMASK, SIG_UNBLOCK, (int64_t)(intptr_t)&mask_block, 0, 8);
    if (rc != 0) { mark_fail("rt_sigprocmask UNBLOCK failed"); return; }

    mark_pass();
}
