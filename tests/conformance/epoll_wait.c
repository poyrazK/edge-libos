// epoll_wait(2) — synchronous test: empty registration set returns 0
// after the timeout, and an unknown epfd returns -EBADF.

#include "syscall.h"

struct epoll_event {
    uint32_t events;
    uint64_t data;
};

void _start(void) {
    int64_t epfd = sc1(NR_EPOLL_CREATE1, 0);
    if (epfd < 0) {
        mark_fail("epoll_create1");
        return;
    }

    struct epoll_event evs[4];
    // timeout = 5ms — empty registrations → returns 0 after sleep.
    int64_t n = sc4(NR_EPOLL_WAIT, epfd, (int64_t)(intptr_t)evs, 4, 5);
    if (n != 0) {
        mark_fail("empty wait");
        return;
    }

    // Unknown epfd → -EBADF.
    int64_t n2 = sc4(NR_EPOLL_WAIT, 9999, (int64_t)(intptr_t)evs, 4, 0);
    if (n2 != -9 /* EBADF */) {
        mark_fail("bad epfd");
        return;
    }

    mark_pass();
}