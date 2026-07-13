// select(2) — translates fd_set bitmasks into a pollfd array internally.
//
// We create a pipe, mark the read end in readfds, and call select with
// a 500 ms timeval timeout. Then we write a byte and call select again;
// it must return 1 with the read-fd bit set in readfds.

#include "syscall.h"

#define FD_SET_SIZE 128   // 16 × uint64

struct timeval { int64_t tv_sec; int64_t tv_usec; };

static void fd_zero(char *set) {
    for (int i = 0; i < FD_SET_SIZE; i++) set[i] = 0;
}
static void fd_set_bit(char *set, int fd) {
    int word = fd >> 6;
    int bit  = fd & 63;
    int off  = word * 8;
    int v = ((int)set[off])
          | (((int)set[off+1]) << 8)
          | (((int)set[off+2]) << 16)
          | (((int)set[off+3]) << 24);
    v |= (1 << bit);
    set[off]   = (char)(v & 0xff);
    set[off+1] = (char)((v >> 8) & 0xff);
    set[off+2] = (char)((v >> 16) & 0xff);
    set[off+3] = (char)((v >> 24) & 0xff);
}
static int fd_isset(const char *set, int fd) {
    int word = fd >> 6;
    int bit  = fd & 63;
    int off  = word * 8;
    int v = ((int)set[off])
          | (((int)set[off+1]) << 8)
          | (((int)set[off+2]) << 16)
          | (((int)set[off+3]) << 24);
    return (v >> bit) & 1;
}

__attribute__((visibility("default")))
void _start(void) {
    int fds[2];
    int64_t pr = sc2(NR_PIPE2, (int64_t)(intptr_t)fds, 0);
    if (pr != 0) {
        mark_fail("pipe2");
        return;
    }
    int rd = fds[0];
    int wr = fds[1];

    // readfds at MARKER_ADDR + 100 (128 bytes), zeroed, then bit rd set.
    char *readfds = (char *)(intptr_t)(MARKER_ADDR + 100);
    fd_zero(readfds);
    fd_set_bit(readfds, rd);

    // timeval at MARKER_ADDR + 400: { sec=0, usec=500_000 } = 500 ms.
    struct timeval *tv = (struct timeval *)(intptr_t)(MARKER_ADDR + 400);
    tv->tv_sec = 0;
    tv->tv_usec = 500 * 1000;

    // First select: empty pipe, 500 ms timeout → returns 0; readfds cleared.
    int64_t r1 = sc5(NR_SELECT, rd + 1 /*nfds*/,
                     (int64_t)(intptr_t)readfds,
                     0 /*writefds*/, 0 /*exceptfds*/,
                     (int64_t)(intptr_t)tv);
    if (r1 < 0) {
        mark_fail("first select error");
        return;
    }
    if (r1 != 0) {
        mark_fail("first select should be 0 on empty pipe");
        return;
    }
    if (fd_isset(readfds, rd)) {
        mark_fail("readfds should be cleared on no-event");
        return;
    }

    // Wake: nanosleep 50 ms, write a byte.
    struct { int64_t sec; int64_t nsec; } *ts =
        (void *)(intptr_t)(MARKER_ADDR + 500);
    ts->sec = 0;
    ts->nsec = 50 * 1000 * 1000;
    sc2(NR_NANOSLEEP, (int64_t)(intptr_t)ts, 0);
    char c = 'z';
    sc3(NR_WRITE, wr, (int64_t)(intptr_t)&c, 1);

    // Second select: must return 1 with readfds bit set.
    fd_zero(readfds);
    fd_set_bit(readfds, rd);
    int64_t r2 = sc5(NR_SELECT, rd + 1, (int64_t)(intptr_t)readfds,
                     0, 0, (int64_t)(intptr_t)tv);
    if (r2 != 1) {
        mark_fail("second select should be 1 with POLLIN");
        return;
    }
    if (!fd_isset(readfds, rd)) {
        mark_fail("readfds bit should be set on read-ready");
        return;
    }

    // Negative: nfds=0 → returns 0 immediately (no work to do).
    int64_t r0 = sc5(NR_SELECT, 0, 0, 0, 0, 0);
    if (r0 != 0) {
        mark_fail("select(nfds=0) should return 0");
        return;
    }

    sc1(NR_CLOSE, rd);
    sc1(NR_CLOSE, wr);
    mark_pass();
}