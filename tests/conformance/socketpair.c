// socketpair(2) — create a connected AF_UNIX pair, then write a byte on
// one side and read it back on the other. This exercises the full
// AF_UNIX stream path.

#include "syscall.h"

__attribute__((visibility("default")))
void _start(void) {
    int sv[2];
    int64_t rc = sc4(NR_SOCKETPAIR, 1 /*AF_UNIX*/, 1 /*SOCK_STREAM*/, 0, (int64_t)(intptr_t)sv);
    if (rc != 0) {
        mark_fail("socketpair failed");
        return;
    }
    int a = sv[0];
    int b = sv[1];
    if (a < 3 || b < 3 || a == b) {
        mark_fail("socketpair returned bogus fds");
        return;
    }

    // Write "x" to a.
    char c = 'x';
    int64_t wr = sc3(NR_WRITE, (uint32_t)a, (int64_t)(intptr_t)&c, 1);
    if (wr != 1) {
        mark_fail("write to socketpair half failed");
        return;
    }

    // Read it back from b.
    char out;
    int64_t rd = sc3(NR_READ, (uint32_t)b, (int64_t)(intptr_t)&out, 1);
    if (rd != 1 || out != 'x') {
        mark_fail("read from socketpair half failed");
        return;
    }

    // Bad family → -EAFNOSUPPORT.
    int sv2[2];
    int64_t rc_bad = sc4(NR_SOCKETPAIR, 2 /*AF_INET*/, 1, 0, (int64_t)(intptr_t)sv2);
    if (rc_bad != -97 /*EAFNOSUPPORT*/) {
        mark_fail("socketpair(AF_INET) should return EAFNOSUPPORT");
        return;
    }

    sc1(NR_CLOSE, (uint32_t)a);
    sc1(NR_CLOSE, (uint32_t)b);
    mark_pass();
}