// shutdown(2) with bad `how` returns -EINVAL.
// shutdown on a non-socket fd returns -EBADF.
// shutdown with SHUT_RD on a fresh socket returns 0 (records intent;
// recvfrom after SHUT_RD is tested at the WAT level).
#include "syscall.h"

__attribute__((visibility("default")))
void _start(void) {
    int64_t fd = sc3(NR_SOCKET, 2 /*AF_INET*/, 1 /*SOCK_STREAM*/, 0);
    if (fd < 3) { mark_fail("socket failed"); return; }

    // Bad `how` → -EINVAL.
    int64_t rc = sc2(NR_SHUTDOWN, fd, 99 /*bogus how*/);
    if (rc != -22 /*EINVAL*/) {
        mark_fail("shutdown bad how should return -EINVAL");
        return;
    }

    // SHUT_RD on a fresh socket → 0.
    rc = sc2(NR_SHUTDOWN, fd, 0 /*SHUT_RD*/);
    if (rc != 0) {
        mark_fail("shutdown SHUT_RD should return 0");
        return;
    }

    // SHUT_WR on a fresh socket → 0.
    rc = sc2(NR_SHUTDOWN, fd, 1 /*SHUT_WR*/);
    if (rc != 0) {
        mark_fail("shutdown SHUT_WR should return 0");
        return;
    }

    // SHUT_RDWR on a fresh socket → 0.
    rc = sc2(NR_SHUTDOWN, fd, 2 /*SHUT_RDWR*/);
    if (rc != 0) {
        mark_fail("shutdown SHUT_RDWR should return 0");
        return;
    }

    // shutdown on a non-socket fd (stdin) → -EBADF.
    rc = sc2(NR_SHUTDOWN, 0 /*stdin*/, 0);
    if (rc != -9 /*EBADF*/) {
        mark_fail("shutdown on stdin should return -EBADF");
        return;
    }

    mark_pass();
}