// getsockname(2) on a bound socket writes back AF_INET,127.0.0.1,bound port.
// getpeername on the same fd (no peer) returns -ENOTCONN.
#include "syscall.h"

__attribute__((visibility("default")))
void _start(void) {
    int64_t fd = sc3(NR_SOCKET, 2 /*AF_INET*/, 1 /*SOCK_STREAM*/, 0);
    if (fd < 3) { mark_fail("socket failed"); return; }

    // sockaddr_in at 4096 (16 bytes).
    char *p = (char *)(intptr_t)4096;
    p[0] = 0x02; p[1] = 0x00;             // AF_INET
    p[2] = 0x1f; p[3] = 0x90;             // port 8080 BE
    p[4] = 0x7f; p[5] = 0x00; p[6] = 0x00; p[7] = 0x01; // 127.0.0.1
    for (int i = 8; i < 16; i++) p[i] = 0;

    int64_t rc = sc3(NR_BIND, fd, (int64_t)(intptr_t)4096, 16);
    if (rc != 0) { mark_fail("bind failed"); return; }

    // getsockname writes to 4200 (16 bytes), addrlen to 4216 (4 bytes).
    char *out = (char *)(intptr_t)4200;
    for (int i = 0; i < 16; i++) out[i] = 0;
    int *outlen = (int *)(intptr_t)4216;
    *outlen = 0;

    rc = sc3(NR_GETSOCKNAME, fd, (int64_t)(intptr_t)4200, (int64_t)(intptr_t)4216);
    if (rc != 0) { mark_fail("getsockname non-zero rc"); return; }

    // Check family == AF_INET.
    if ((out[0] & 0xff) != 0x02 || (out[1] & 0xff) != 0x00) {
        mark_fail("getsockname family != AF_INET");
        return;
    }
    // Check port == 8080 (BE).
    if ((out[2] & 0xff) != 0x1f || (out[3] & 0xff) != 0x90) {
        mark_fail("getsockname port != 8080");
        return;
    }
    // Check addr == 127.0.0.1.
    if ((out[4] & 0xff) != 0x7f || (out[5] & 0xff) != 0x00 ||
        (out[6] & 0xff) != 0x00 || (out[7] & 0xff) != 0x01) {
        mark_fail("getsockname addr != 127.0.0.1");
        return;
    }
    // Check addrlen written back as 16.
    if (*outlen != 16) {
        mark_fail("getsockname addrlen != 16");
        return;
    }

    // getpeername without prior accept/connect → -ENOTCONN.
    rc = sc3(NR_GETPEERNAME, fd, (int64_t)(intptr_t)4200, (int64_t)(intptr_t)4216);
    if (rc != -107 /*ENOTCONN*/) {
        mark_fail("getpeername unbound should return -ENOTCONN");
        return;
    }

    mark_pass();
}