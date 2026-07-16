// bind(2) on an AF_INET SOCK_STREAM socket bound to 127.0.0.1:8080. Returns 0.
#include "syscall.h"

__attribute__((visibility("default")))
void _start(void) {
    // First create a socket.
    int64_t fd = sc3(NR_SOCKET, 2 /*AF_INET*/, 1 /*SOCK_STREAM*/, 0);
    if (fd < 3) {
        mark_fail("socket returned invalid fd");
        return;
    }

    // Build a sockaddr_in in marker memory after "PASS\0" at offset 4096+5,
    // placing it at 4100 to avoid stomping on the marker bytes. 16 bytes.
    char *p = (char *)(intptr_t)4100;
    // family = AF_INET (2 LE)
    p[0] = 0x02; p[1] = 0x00;
    // port = 8080 (BE)
    p[2] = 0x1f; p[3] = 0x90;
    // addr = 127.0.0.1
    p[4] = 0x7f; p[5] = 0x00; p[6] = 0x00; p[7] = 0x01;
    // 8 bytes of padding
    for (int i = 8; i < 16; i++) p[i] = 0;

    int64_t rc = sc3(NR_BIND, fd, (int64_t)(intptr_t)4100, 16);
    if (rc == -98 /*EADDRINUSE*/) {
        // Port 8080 is held by another host process on this machine —
        // not a kernel bug. Degrade to SKIP so CI doesn't trip when
        // the test harness runs alongside a stray dev server.
        mark_skip("port 8080 in use");
        return;
    }
    if (rc == 0) mark_pass();
    else mark_fail("bind returned non-zero");
}
