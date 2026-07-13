// epoll_pwait(2) — like epoll_wait but takes a struct timespec timeout
// (and a sigmask that we ignore).
//
// Empty registration set → returns 0 after the timeout.

#include "syscall.h"

struct epoll_event {
    uint32_t events;
    uint64_t data;
};

struct timespec { int64_t tv_sec; int64_t tv_nsec; };

__attribute__((visibility("default")))
void _start(void) {
    int64_t epfd = sc1(NR_EPOLL_CREATE1, 0);
    if (epfd < 0) {
        mark_fail("epoll_create1");
        return;
    }

    struct epoll_event evs[4];

    // timespec at MARKER_ADDR + 100: { sec=0, nsec=5_000_000 } = 5 ms.
    struct timespec *ts = (struct timespec *)(intptr_t)(MARKER_ADDR + 100);
    ts->tv_sec = 0;
    ts->tv_nsec = 5 * 1000 * 1000;

    // Empty set, 5 ms timeout → returns 0.
    int64_t n = sc6(NR_EPOLL_PWAIT, epfd,
                    (int64_t)(intptr_t)evs, 4 /*maxevents*/,
                    (int64_t)(intptr_t)ts,
                    0 /*sigmask*/, 8 /*sigsetsize*/);
    if (n != 0) {
        mark_fail("empty pwait should return 0");
        return;
    }

    // NULL timespec on empty set with no event would block forever;
    // use timeout=0 (NULL timespec) → returns immediately with 0 events.
    // NULL timespec means "wait forever" per our ppoll semantics, so we
    // don't exercise that here.

    // Bad epfd → -EBADF.
    int64_t n2 = sc6(NR_EPOLL_PWAIT, 9999, (int64_t)(intptr_t)evs, 4,
                     0, 0, 8);
    if (n2 != -9 /*EBADF*/) {
        mark_fail("bad epfd should return EBADF");
        return;
    }

    sc1(NR_CLOSE, epfd);
    mark_pass();
}