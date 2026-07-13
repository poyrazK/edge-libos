// fcntl(rd, F_DUPFD, MIN) where MIN is below next_fd.
//
// Regression guard for the BLOCKER caught in PR #4 review:
// insert_at_least used to scan from max(min, next_fd), but Linux
// semantics is "lowest free fd ≥ min" regardless of next_fd.
//
// Setup: pipe2(3,4). dup(3) → 5, dup(3) → 6 (next_fd=7). Close 3..=6
// so the table has holes 3..6 below next_fd=7. The KEEP fd is 5 (a
// stable dup of the read end). F_DUPFD(KEEP, 3) must return 3 (lowest
// free), NOT 7 (the buggy implementation skipped ahead).
#include "syscall.h"

__attribute__((visibility("default")))
void _start(void) {
    int32_t *fds_arr = (int32_t *)(intptr_t)MARKER_ADDR;
    int64_t pipe2_ret = sc2(NR_PIPE2, (int64_t)(intptr_t)fds_arr, 0);
    if (pipe2_ret != 0) { mark_fail("pipe2 failed"); return; }
    int rd = (int)fds_arr[0];
    int wr = (int)fds_arr[1];

    // dup the read end so we can close 3..6 safely.
    int64_t d1 = sc1(NR_DUP, rd);
    if (d1 != 5) { mark_fail("dup1 didn't return 5"); return; }
    int64_t d2 = sc1(NR_DUP, rd);
    if (d2 != 6) { mark_fail("dup2 didn't return 6"); return; }
    int keep = 5;  // a stable read-end fd; safe to use after closing 3.

    // Close 3..=6 so the table has holes. next_fd stays 7.
    if (sc1(3 /* NR_CLOSE */, 3) != 0) { mark_fail("close(3)"); return; }
    if (sc1(3 /* NR_CLOSE */, 4) != 0) { mark_fail("close(4)"); return; }
    if (sc1(3 /* NR_CLOSE */, 6) != 0) { mark_fail("close(6)"); return; }

    // F_DUPFD(keep, 3). Per Linux: lowest free ≥ 3. With 3 free, must
    // return 3, NOT 7 (the buggy implementation would skip ahead).
    int64_t r = sc3(NR_FCNTL, keep, F_DUPFD, 3);
    if (r != 3) { mark_fail("F_DUPFD(keep,3) didn't return 3 with holes below next_fd"); return; }

    mark_pass();
}