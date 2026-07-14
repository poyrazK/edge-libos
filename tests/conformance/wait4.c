// P3 Tier-6 wait4(61) v1 conformance:
//   - wait4(any, NULL, WNOHANG, NULL) on a kernel with no children → -ECHILD
//     (Linux semantics: ECHILD when caller has no children, regardless of WNOHANG).
//   - wait4(99999, NULL, WNOHANG, NULL) for a non-existent PID → -ECHILD.
//   - wait4(any, NULL, 0, NULL) without WNOHANG, no children → -ECHILD
//     (v1 has no parked-wait path; PR 4 lands the blocking variant).
//   - wait4(any, NULL, WUNTRACED, NULL) → -EINVAL (unsupported flag).
//   - wait4(-2, NULL, WNOHANG, NULL) → -EINVAL (process-group pid not supported in v1).
//   - wait4(any, &wstatus, WNOHANG, NULL) on no-children path must NOT touch the slot.

#include "syscall.h"

#define WNOHANG     0x40
#define WUNTRACED   0x02

static int wstatus __attribute__((aligned(8)));

__attribute__((visibility("default")))
void _start(void) {
    // Case 1: any-child + WNOHANG, no children exist → -ECHILD.
    int64_t r = sc4(NR_WAIT4, 0, 0, (int64_t)WNOHANG, 0);
    if (r != -10) { mark_fail("wait4(any, WNOHANG) no children != -ECHILD"); return; }

    // Case 2: non-existent PID + WNOHANG → -ECHILD.
    r = sc4(NR_WAIT4, 99999, 0, (int64_t)WNOHANG, 0);
    if (r != -10) { mark_fail("wait4(99999, WNOHANG) != -ECHILD"); return; }

    // Case 3: any-child, no WNOHANG, no children → -ECHILD.
    r = sc4(NR_WAIT4, 0, 0, 0, 0);
    if (r != -10) { mark_fail("wait4(any, no WNOHANG, no children) != -ECHILD"); return; }

    // Case 4: unsupported flag WUNTRACED → -EINVAL.
    r = sc4(NR_WAIT4, 0, 0, (int64_t)WUNTRACED, 0);
    if (r != -22) { mark_fail("wait4(WUNTRACED) != -EINVAL"); return; }

    // Case 5: process-group pid (< -1) → -EINVAL.
    r = sc4(NR_WAIT4, -2, 0, (int64_t)WNOHANG, 0);
    if (r != -22) { mark_fail("wait4(-2, …) != -EINVAL"); return; }

    // Case 6: wstatus pointer writeback path. No-children branch must
    // NOT touch the slot (we read sentinel = 0x42).
    wstatus = 0x42;
    r = sc4(NR_WAIT4, 0, (int64_t)&wstatus, (int64_t)WNOHANG, 0);
    if (r != -10) { mark_fail("wait4(any, &wstatus, WNOHANG) no children != -ECHILD"); return; }
    if (wstatus != 0x42) { mark_fail("wait4 no-children path must not write wstatus"); return; }

    mark_pass();
}