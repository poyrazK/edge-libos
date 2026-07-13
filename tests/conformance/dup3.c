// dup3(rd, NEW_FD, O_CLOEXEC) → NEW_FD with FD_CLOEXEC set.
//
// Validates:
//   - dup3 returns NEW_FD
//   - fcntl(NEW_FD, F_GETFD) returns 1 (cloexec is on)
//   - the duplicate shares the read end's buffer state
#include "syscall.h"

#define NEW_FD 9

__attribute__((visibility("default")))
void _start(void) {
    int32_t *fds_arr = (int32_t *)(intptr_t)MARKER_ADDR;
    int64_t pipe2_ret = sc2(NR_PIPE2, (int64_t)(intptr_t)fds_arr, 0);
    if (pipe2_ret != 0) { mark_fail("pipe2 failed"); return; }
    int rd = (int)fds_arr[0];
    int wr = (int)fds_arr[1];

    // Write a byte so we can confirm the duplicate sees the same buffer.
    char *one = (char *)(intptr_t)(MARKER_ADDR + 4096);
    one[0] = 'Z';
    int64_t w = sc3(NR_WRITE, wr, (int64_t)(intptr_t)one, 1);
    if (w != 1) { mark_fail("write failed"); return; }

    // dup3(rd, NEW_FD, O_CLOEXEC).
    int64_t d = sc3(NR_DUP3, rd, NEW_FD, O_CLOEXEC);
    if (d != NEW_FD) { mark_fail("dup3 didn't return target fd"); return; }

    // The dup'd fd must have FD_CLOEXEC set (fd table reports 1).
    int32_t *clo = (int32_t *)(intptr_t)(MARKER_ADDR + 8192);
    int64_t getfd = sc3(NR_FCNTL, NEW_FD, F_GETFD, (int64_t)(intptr_t)clo);
    // Note: F_GETFD returns the bit value itself (0 or 1) and writes the
    // same value into the buffer. Failure of the syscall would manifest
    // as a negative return.
    if (getfd < 0) { mark_fail("fcntl F_GETFD returned errno"); return; }
    if (clo[0] != 1) { mark_fail("FD_CLOEXEC wasn't set by dup3"); return; }

    // Read from NEW_FD: should consume the shared byte.
    char *buf = (char *)(intptr_t)(MARKER_ADDR + 16384);
    int64_t r = sc3(NR_READ, NEW_FD, (int64_t)(intptr_t)buf, 1);
    if (r != 1 || buf[0] != 'Z') { mark_fail("dup'd fd did not read shared byte"); return; }

    mark_pass();
}
