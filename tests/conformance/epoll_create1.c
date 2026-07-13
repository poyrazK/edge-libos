// epoll_create1(2) — allocate an epoll instance, verify it returns a
// positive fd, then close it.
//
// P1-7: only the `flags == 0` path is exercised. `EPOLL_CLOEXEC` would
// be accepted but is recorded-only; P1 doesn't model exec.

#include "syscall.h"

void _start(void) {
    int64_t epfd = sc1(NR_EPOLL_CREATE1, 0);
    if (epfd < 0) {
        mark_fail("epoll_create1");
        return;
    }

    // Try to close it via NR_CLOSE (NR_CLOSE = 3).
    int64_t rc = sc1(NR_CLOSE, epfd);
    if (rc != 0) {
        mark_fail("close epfd");
        return;
    }

    mark_pass();
}