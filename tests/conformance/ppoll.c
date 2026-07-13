// ppoll(2) — translates a `struct timespec` timeout and delegates to poll().
//
// We mirror the poll_timeout test: create a pipe, call ppoll with a
// 500ms timespec while the read end is empty; then nanosleep 50ms,
// write a byte, and call ppoll again. The second call must return 1
// with POLLIN.

#include "syscall.h"

struct timespec { int64_t tv_sec; int64_t tv_nsec; };

__attribute__((visibility("default")))
void _start(void) {
    int fds[2];
    int64_t pr = sc2(NR_PIPE2, (int64_t)(intptr_t)fds, 0);
    if (pr != 0) {
        mark_fail("pipe2 failed");
        return;
    }
    int rd = fds[0];
    int wr = fds[1];

    // pollfd at MARKER_ADDR + 200.
    char *pf = (char *)(intptr_t)(MARKER_ADDR + 200);
    pf[0] = (char)(rd & 0xff);
    pf[1] = (char)((rd >> 8) & 0xff);
    pf[2] = (char)((rd >> 16) & 0xff);
    pf[3] = (char)((rd >> 24) & 0xff);
    pf[4] = 0x01; pf[5] = 0x00; // events = POLLIN

    // timespec at MARKER_ADDR + 100: { sec=0, nsec=500_000_000 } = 500 ms.
    struct timespec *ts = (struct timespec *)(intptr_t)(MARKER_ADDR + 100);
    ts->tv_sec = 0;
    ts->tv_nsec = 500 * 1000 * 1000;

    // First ppoll: empty pipe, 500 ms timeout → returns 0 (no events).
    int64_t r1 = sc5(NR_PPOLL, (int64_t)(intptr_t)pf, 1 /*nfds*/,
                     (int64_t)(intptr_t)ts, 0 /*sigmask*/, 8 /*sigsetsize*/);
    if (r1 < 0) {
        mark_fail("first ppoll error");
        return;
    }

    // Wake: sleep 50 ms then write a byte.
    ts->tv_sec = 0;
    ts->tv_nsec = 50 * 1000 * 1000;
    sc2(NR_NANOSLEEP, (int64_t)(intptr_t)ts, 0);

    char c = 'y';
    sc3(NR_WRITE, wr, (int64_t)(intptr_t)&c, 1);

    // Second ppoll: must return 1 with POLLIN.
    pf[6] = 0x00; pf[7] = 0x00; // clear revents
    int64_t r2 = sc5(NR_PPOLL, (int64_t)(intptr_t)pf, 1,
                     (int64_t)(intptr_t)ts, 0, 8);
    if (r2 != 1) {
        mark_fail("second ppoll should return 1 with POLLIN");
        return;
    }
    if (!(pf[6] & 0x01)) {
        mark_fail("revents missing POLLIN");
        return;
    }

    // ppoll with NULL timespec (wait forever) on empty pipe would block
    // forever — instead verify EINVAL on negative nfds.
    int64_t r_bad = sc5(NR_PPOLL, (int64_t)(intptr_t)pf, -1,
                        0, 0, 8);
    if (r_bad != -22 /*EINVAL*/) {
        mark_fail("ppoll(nfds=-1) should return EINVAL");
        return;
    }

    sc1(NR_CLOSE, rd);
    sc1(NR_CLOSE, wr);
    mark_pass();
}