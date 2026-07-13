// getsockopt(2) on a freshly-created socket.
// Honors SO_TYPE=1 (SOCK_STREAM) and SO_DOMAIN=2 (AF_INET).
// Unknown opts return 0 (mirrors setsockopt semantics).
#include "syscall.h"

__attribute__((visibility("default")))
void _start(void) {
    int64_t fd = sc3(NR_SOCKET, 2 /*AF_INET*/, 1 /*SOCK_STREAM*/, 0);
    if (fd < 3) {
        mark_fail("socket returned invalid fd");
        return;
    }

    // Output buffers at 4096.
    int *optval = (int *)(intptr_t)4096;
    int *optlen = (int *)(intptr_t)4100;

    // SO_TYPE → 1 (SOCK_STREAM).
    optval[0] = 0xdeadbeef;
    int64_t rc = sc5(NR_GETSOCKOPT, fd, 1 /*SOL_SOCKET*/, 3 /*SO_TYPE*/,
                     (int64_t)(intptr_t)optval, (int64_t)(intptr_t)optlen);
    if (rc != 0) { mark_fail("getsockopt SO_TYPE non-zero rc"); return; }
    if (optval[0] != 1) {
        mark_fail("getsockopt SO_TYPE != 1");
        return;
    }

    // SO_DOMAIN → 2 (AF_INET).
    optval[0] = 0xdeadbeef;
    rc = sc5(NR_GETSOCKOPT, fd, 1 /*SOL_SOCKET*/, 39 /*SO_DOMAIN*/,
             (int64_t)(intptr_t)optval, (int64_t)(intptr_t)optlen);
    if (rc != 0) { mark_fail("getsockopt SO_DOMAIN non-zero rc"); return; }
    if (optval[0] != 2) {
        mark_fail("getsockopt SO_DOMAIN != 2");
        return;
    }

    // Unknown opt → 0 (mirrors setsockopt semantics).
    optval[0] = 0xdeadbeef;
    rc = sc5(NR_GETSOCKOPT, fd, 999 /*bogus level*/, 999 /*bogus opt*/,
             (int64_t)(intptr_t)optval, (int64_t)(intptr_t)optlen);
    if (rc != 0) { mark_fail("getsockopt unknown non-zero rc"); return; }
    if (optval[0] != 0) {
        mark_fail("getsockopt unknown != 0");
        return;
    }

    // getsockopt on a non-socket fd (stdin) returns -EBADF.
    rc = sc5(NR_GETSOCKOPT, 0 /*stdin*/, 1, 3,
             (int64_t)(intptr_t)optval, (int64_t)(intptr_t)optlen);
    if (rc != -9 /*EBADF*/) {
        mark_fail("getsockopt on stdin should return -EBADF");
        return;
    }

    mark_pass();
}