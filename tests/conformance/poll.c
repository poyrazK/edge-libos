// poll(2) on a single fd that doesn't exist.
// struct pollfd at offset 4096 (8 bytes per entry).
//
// Layout (LE):
//   +0: int fd
//   +4: short events
//   +6: short revents
#include "syscall.h"

__attribute__((visibility("default")))
void _start(void) {
    // Single pollfd at 4096: fd=9999 (unknown), events=POLLIN|1, revents=0.
    char *p = (char *)(intptr_t)4096;
    p[0] = 0x0f; p[1] = 0x27; p[2] = 0x00; p[3] = 0x00; // fd=9999
    p[4] = 0x01; p[5] = 0x00;                          // events=POLLIN
    p[6] = 0x00; p[7] = 0x00;                          // revents=0 in

    int64_t rc = sc3(NR_POLL, (int64_t)(intptr_t)4096, 1 /*nfds*/, 0 /*timeout*/);
    // poll returns count of fds with non-zero revents. POLLNVAL is non-zero,
    // so for an unknown fd we should get >= 1.
    if (rc < 1) {
        mark_fail("poll on unknown fd should return >= 1");
        return;
    }

    // Confirm revents now has POLLNVAL (0x0020) at offset 6.
    short revents;
    __builtin_memcpy(&revents, &p[6], 2);
    if ((revents & 0x0020 /*POLLNVAL*/) == 0) {
        mark_fail("poll revents missing POLLNVAL");
        return;
    }

    // nfds=0 returns 0.
    rc = sc3(NR_POLL, (int64_t)(intptr_t)4096, 0, 0);
    if (rc != 0) {
        mark_fail("poll with nfds=0 should return 0");
        return;
    }

    // Negative nfds → -EINVAL.
    rc = sc3(NR_POLL, (int64_t)(intptr_t)4096, -1, 0);
    if (rc != -22 /*EINVAL*/) {
        mark_fail("poll with nfds=-1 should return -EINVAL");
        return;
    }

    mark_pass();
}