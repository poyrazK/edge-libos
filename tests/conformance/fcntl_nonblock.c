// fcntl(F_SETFL, O_NONBLOCK) on the read end of a freshly created pipe2
// pair should make a subsequent `read` on that fd return -EAGAIN when the
// pipe buffer is empty. We verify both halves:
//   (1) pipe2(fds, O_NONBLOCK) yields a nonblocking pair.
//   (2) read(rd_fd, buf, 16) returns -EAGAIN.
#include "syscall.h"

__attribute__((visibility("default")))
void _start(void) {
    // Place the fdarray at offset 4100 (8 bytes).
    int64_t rc = sc2(NR_PIPE2, (int64_t)(intptr_t)4100, 04000 /*O_NONBLOCK*/);
    if (rc != 0) {
        mark_fail("pipe2(O_NONBLOCK) failed");
        return;
    }

    // Decode fds.
    int32_t *fds = (int32_t *)(intptr_t)4100;
    int32_t rd_fd = fds[0];
    int32_t wr_fd = fds[1];

    // F_GETFL on the read end should report O_RDONLY | O_NONBLOCK.
    rc = sc3(NR_FCNTL, rd_fd, 3 /*F_GETFL*/, 0);
    if (rc != (0 | 04000)) {
        mark_fail("F_GETFL did not report O_NONBLOCK on pipe read end");
        return;
    }

    // Place a 16-byte read buffer at offset 4200.
    char *buf = (char *)(intptr_t)4200;
    for (int i = 0; i < 16; i++) buf[i] = 0;

    rc = sc3(0 /*NR_READ*/, rd_fd, (int64_t)(intptr_t)4200, 16);
    if (rc != -11 /*EAGAIN*/) {
        mark_fail("nonblocking read on empty pipe did not return -EAGAIN");
        return;
    }

    // F_SETFL with O_NONBLOCK=0 should clear the bit.
    rc = sc3(NR_FCNTL, rd_fd, 4 /*F_SETFL*/, 0);
    if (rc != 0) {
        mark_fail("F_SETFL=0 failed");
        return;
    }
    rc = sc3(NR_FCNTL, rd_fd, 3 /*F_GETFL*/, 0);
    if (rc & 04000) {
        mark_fail("O_NONBLOCK was not cleared after F_SETFL=0");
        return;
    }

    // Suppress wr_fd unused warning.
    (void)wr_fd;

    mark_pass();
}