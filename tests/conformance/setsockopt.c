// setsockopt(SOL_SOCKET, SO_REUSEADDR, &one, 4) on a fresh AF_INET stream
// socket returns 0. Then setsockopt(IPPROTO_TCP, TCP_NODELAY, &one, 4)
// also returns 0. Both are recorded on the SocketInner but the kernel
// doesn't yet surface them to listeners — they're accepted at the ABI.
#include "syscall.h"

__attribute__((visibility("default")))
void _start(void) {
    int64_t fd = sc3(NR_SOCKET, 2 /*AF_INET*/, 1 /*SOCK_STREAM*/, 0);
    if (fd < 3) {
        mark_fail("socket returned invalid fd");
        return;
    }

    // Place optval (4 bytes = 1) at offset 4100.
    char *p = (char *)(intptr_t)4100;
    p[0] = 1; p[1] = 0; p[2] = 0; p[3] = 0;

    int64_t rc = sc5(NR_SETSOCKOPT, fd, 1 /*SOL_SOCKET*/, 2 /*SO_REUSEADDR*/,
                     (int64_t)(intptr_t)4100, 4);
    if (rc != 0) {
        mark_fail("setsockopt(SO_REUSEADDR) failed");
        return;
    }

    rc = sc5(NR_SETSOCKOPT, fd, 6 /*IPPROTO_TCP*/, 1 /*TCP_NODELAY*/,
             (int64_t)(intptr_t)4100, 4);
    if (rc != 0) {
        mark_fail("setsockopt(TCP_NODELAY) failed");
        return;
    }

    // Also accept an unknown (level, optname) — plan §4.4: → 0.
    rc = sc5(NR_SETSOCKOPT, fd, 999 /*unknown level*/, 999 /*unknown opt*/,
             (int64_t)(intptr_t)4100, 4);
    if (rc != 0) {
        mark_fail("unknown setsockopt returned non-zero");
        return;
    }

    mark_pass();
}