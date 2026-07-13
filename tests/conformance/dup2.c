// dup2(rd, NEW_FD) → exact fd at NEW_FD, sharing the read end's state.
//
// Validates:
//   - dup2 returns NEW_FD (not lowest-free)
//   - a subsequent close of the duplicated fd succeeds (proves it was bound)
//   - the duplicate shares the pipe's offset (we can read the same byte)
#include "syscall.h"

#define NEW_FD 7

__attribute__((visibility("default")))
void _start(void) {
    int32_t *fds_arr = (int32_t *)(intptr_t)MARKER_ADDR;
    int64_t pipe2_ret = sc2(NR_PIPE2, (int64_t)(intptr_t)fds_arr, 0);
    if (pipe2_ret != 0) { mark_fail("pipe2 failed"); return; }
    int rd = (int)fds_arr[0];
    int wr = (int)fds_arr[1];

    // Write one byte.
    char *one = (char *)(intptr_t)(MARKER_ADDR + 4096);
    one[0] = 'A';
    int64_t w = sc3(NR_WRITE, wr, (int64_t)(intptr_t)one, 1);
    if (w != 1) { mark_fail("write failed"); return; }

    // dup2(rd, NEW_FD).
    int64_t dup2_ret = sc2(NR_DUP2, rd, NEW_FD);
    if (dup2_ret != NEW_FD) { mark_fail("dup2 did not return target fd"); return; }

    // Read from the original rd; should consume the shared offset.
    char *buf = (char *)(intptr_t)(MARKER_ADDR + 8192);
    int64_t r1 = sc3(NR_READ, rd, (int64_t)(intptr_t)buf, 1);
    if (r1 != 1 || buf[0] != 'A') { mark_fail("read from rd"); return; }

    // Write another byte; the dup'd NEW_FD should now see it because the
    // pipe's read-offset was consumed only after the first read.
    one[0] = 'B';
    int64_t w2 = sc3(NR_WRITE, wr, (int64_t)(intptr_t)one, 1);
    if (w2 != 1) { mark_fail("second write failed"); return; }

    char *buf2 = (char *)(intptr_t)(MARKER_ADDR + 8192 + 16);
    int64_t r2 = sc3(NR_READ, NEW_FD, (int64_t)(intptr_t)buf2, 1);
    if (r2 != 1 || buf2[0] != 'B') { mark_fail("dup'd fd did not see byte"); return; }

    // Cleanup: close the dup'd fd.
    int64_t c = sc1(3 /* NR_CLOSE */, NEW_FD);
    if (c != 0) { mark_fail("close failed"); return; }

    mark_pass();
}
