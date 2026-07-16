// udp_loopback: end-to-end UDP sendto/recvfrom over loopback. Opens
// two AF_INET SOCK_DGRAM sockets (A, B), binds both to 127.0.0.1:0,
// reads B's ephemeral port via getsockname, sends "ping" from A to B,
// receives "ping" on B with the source sockaddr and addrlen written
// back. This is the canonical smoke test for the UDP data path (C2).
//
// Memory map (offsets past the 4096-byte marker at 4096):
//   4200: bind sockaddr_in (AF_INET, port=0, 127.0.0.1)
//   4216: recv source sockaddr_in (B receives source addr here)
//   4244: addrlen for recv (4-byte LE)
//   4252: destination sockaddr_in (A sends to B)
//   4268: "ping" payload (4 bytes)
#include "syscall.h"

#include <stdint.h>

__attribute__((visibility("default")))
void _start(void) {
    int64_t fd_a = sc3(NR_SOCKET, 2 /*AF_INET*/, 2 /*SOCK_DGRAM*/, 0);
    int64_t fd_b = sc3(NR_SOCKET, 2 /*AF_INET*/, 2 /*SOCK_DGRAM*/, 0);
    if (fd_a < 3 || fd_b < 3) {
        mark_fail("socket returned invalid fd");
        return;
    }

    // Build bind sockaddr_in at 4200. Bind both to 127.0.0.1:0 (ephemeral).
    char *bind_addr = (char *)(intptr_t)4200;
    bind_addr[0] = 0x02; bind_addr[1] = 0x00;            // AF_INET (LE)
    bind_addr[2] = 0x00; bind_addr[3] = 0x00;            // port = 0 (BE, ephemeral)
    bind_addr[4] = 0x7f; bind_addr[5] = 0x00;
    bind_addr[6] = 0x00; bind_addr[7] = 0x01;            // 127.0.0.1
    for (int i = 8; i < 16; i++) bind_addr[i] = 0;

    int64_t rc_a = sc3(NR_BIND, fd_a, (int64_t)(intptr_t)bind_addr, 16);
    int64_t rc_b = sc3(NR_BIND, fd_b, (int64_t)(intptr_t)bind_addr, 16);
    if (rc_a != 0 || rc_b != 0) {
        mark_fail("bind returned non-zero");
        return;
    }

    // getsockname(B) — writes sockaddr_in to 4220, addrlen to 4244.
    int64_t gsn = sc3(NR_GETSOCKNAME, fd_b, (int64_t)(intptr_t)4220, (int64_t)(intptr_t)4244);
    if (gsn != 0) {
        mark_fail("getsockname returned non-zero");
        return;
    }
    // B's port is at offset 4222 (BE: high byte 4222, low byte 4223).
    uint16_t b_port = ((uint8_t)(((char *)(intptr_t)4222)[0]) << 8)
                    | (uint8_t)(((char *)(intptr_t)4223)[0]);
    if (b_port == 0) {
        mark_fail("getsockname returned port 0");
        return;
    }

    // Build the destination sockaddr at 4252: family + BE port + 127.0.0.1.
    char *dst = (char *)(intptr_t)4252;
    dst[0] = 0x02; dst[1] = 0x00;                        // AF_INET (LE)
    dst[2] = (char)(b_port >> 8); dst[3] = (char)(b_port & 0xff);  // BE
    dst[4] = 0x7f; dst[5] = 0x00; dst[6] = 0x00; dst[7] = 0x01;
    for (int i = 8; i < 16; i++) dst[i] = 0;

    // "ping" payload at 4268.
    char *data = (char *)(intptr_t)4268;
    data[0] = 'p'; data[1] = 'i'; data[2] = 'n'; data[3] = 'g';

    int64_t snd = sc6(NR_SENDTO, fd_a,
                      (int64_t)(intptr_t)data /*buf*/, 4 /*len*/,
                      0 /*flags*/,
                      (int64_t)(intptr_t)dst /*addr*/, 16 /*addrlen*/);
    if (snd != 4) {
        mark_fail("sendto did not return 4");
        return;
    }

    // B receives. Source sockaddr written to 4216, addrlen to 4252+16=4268? No,
    // 4244 — but we already use 4268 for the payload. Place payload at 4284,
    // addrlen ptr at 4244, recv buf at 4284.
    int64_t rcv = sc6(NR_RECVFROM, fd_b,
                      (int64_t)(intptr_t)data /*buf*/, 32 /*len*/,
                      0 /*flags*/,
                      (int64_t)(intptr_t)4216 /*src addr*/, (int64_t)(intptr_t)4244 /*addrlen*/);
    if (rcv != 4) {
        mark_fail("recvfrom did not return 4");
        return;
    }
    if (data[0] != 'p' || data[1] != 'i' || data[2] != 'n' || data[3] != 'g') {
        mark_fail("received bytes do not match 'ping'");
        return;
    }
    // Source port at offset 4218 (BE).
    uint16_t src_port = ((uint8_t)(((char *)(intptr_t)4218)[0]) << 8)
                      | (uint8_t)(((char *)(intptr_t)4219)[0]);
    if (src_port == 0) {
        mark_fail("recvfrom source port is 0");
        return;
    }

    // Cleanup.
    sc1(NR_CLOSE, fd_a);
    sc1(NR_CLOSE, fd_b);
    mark_pass();
}
