// connect(2) on a non-socket fd returns -EBADF.
// (We can't easily test the success path via C conformance without a
// host-side peer listener; the WAT-level roundtrip test in
// tests/socket_conformance.rs::sendto_then_recvfrom_roundtrips_over_loopback
// covers that.)
#include "syscall.h"

__attribute__((visibility("default")))
void _start(void) {
    int64_t fd = sc3(NR_SOCKET, 2 /*AF_INET*/, 1 /*SOCK_STREAM*/, 0);
    if (fd < 3) {
        mark_fail("socket returned invalid fd");
        return;
    }

    char *p = (char *)(intptr_t)4100;
    p[0] = 0x02; p[1] = 0x00;             // AF_INET
    p[2] = 0x00; p[3] = 0x01;             // port 1 (BE) — almost certainly closed
    p[4] = 0x7f; p[5] = 0x00; p[6] = 0x00; p[7] = 0x01; // 127.0.0.1
    for (int i = 8; i < 16; i++) p[i] = 0;

    int64_t rc = sc3(NR_CONNECT, fd, (int64_t)(intptr_t)4100, 16);
    // Port 1 should yield ECONNREFUSED; if a firewall drops it could be
    // ETIMEDOUT or EIO. We accept any of the documented error sentinels.
    if (rc == -111 /*ECONNREFUSED*/ ||
        rc == -110 /*ETIMEDOUT*/ ||
        rc == -5   /*EIO*/) {
        mark_pass();
    } else {
        mark_fail("connect to port 1 returned unexpected value");
    }
}