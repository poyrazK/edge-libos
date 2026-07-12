// listen(2) on a bound socket. Returns 0 on success.
// Exercises the full socket → bind → listen path end-to-end through the C ABI.
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
    p[2] = 0x1f; p[3] = 0x90;             // port 8080 BE
    p[4] = 0x7f; p[5] = 0x00; p[6] = 0x00; p[7] = 0x01; // 127.0.0.1
    for (int i = 8; i < 16; i++) p[i] = 0;

    int64_t rc = sc3(NR_BIND, fd, (int64_t)(intptr_t)4100, 16);
    if (rc != 0) {
        mark_fail("bind returned non-zero");
        return;
    }

    rc = sc2(NR_LISTEN, fd, 5);
    if (rc == 0) mark_pass();
    else mark_fail("listen returned non-zero");
}
