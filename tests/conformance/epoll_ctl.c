// epoll_ctl(2) — ADD + DEL an entry on a fresh epoll instance.
//
// We allocate an epoll fd, then ADD a stub fd (NR_GETPID — anything that
// returns a real fd number) with EPOLLIN. ADD should return 0. DEL
// should also return 0. EBADF on unknown op.

#include "syscall.h"

// epoll_event { u32 events; u64 data; }
struct epoll_event {
    uint32_t events;
    uint64_t data;
};

#define EPOLLIN  0x001
#define EPOLLOUT 0x004
#define EPOLL_CTL_ADD 1
#define EPOLL_CTL_DEL 2

void _start(void) {
    int64_t epfd = sc1(NR_EPOLL_CREATE1, 0);
    if (epfd < 0) {
        mark_fail("epoll_create1");
        return;
    }

    // Pick a victim fd that's always valid: STDOUT (fd=1).
    int64_t victim = 1;

    struct epoll_event ev;
    ev.events = EPOLLIN;
    ev.data = 0xdeadbeefcafe;

    int64_t rc = sc4(NR_EPOLL_CTL, epfd, EPOLL_CTL_ADD, victim,
                     (int64_t)(intptr_t)&ev);
    if (rc != 0) {
        mark_fail("epoll_ctl ADD");
        return;
    }

    rc = sc4(NR_EPOLL_CTL, epfd, EPOLL_CTL_DEL, victim, 0);
    if (rc != 0) {
        mark_fail("epoll_ctl DEL");
        return;
    }

    // Re-ADD + DEL again to confirm the slot is reusable.
    rc = sc4(NR_EPOLL_CTL, epfd, EPOLL_CTL_ADD, victim,
             (int64_t)(intptr_t)&ev);
    if (rc != 0) {
        mark_fail("epoll_ctl ADD (re-add)");
        return;
    }
    rc = sc4(NR_EPOLL_CTL, epfd, EPOLL_CTL_DEL, victim, 0);
    if (rc != 0) {
        mark_fail("epoll_ctl DEL (re-del)");
        return;
    }

    // Unknown op → -EINVAL.
    rc = sc4(NR_EPOLL_CTL, epfd, 99, victim, 0);
    if (rc != -22 /* EINVAL */) {
        mark_fail("epoll_ctl bad op");
        return;
    }

    mark_pass();
}