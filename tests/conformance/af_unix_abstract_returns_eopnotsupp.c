// AF_UNIX abstract namespace (sun_path[0] == 0) is explicitly
// unsupported — the kernel surfaces -EOPNOTSUPP so guests fail fast
// instead of getting a misleading ENOENT.
//
// We exercise the rejection by binding a stream socket to an abstract
// address (the path's first byte is NUL). The bind(2) call must return
// -EOPNOTSUPP.

#include "syscall.h"

__attribute__((visibility("default")))
void _start(void) {
    int fd = sc3(NR_SOCKET, 1 /*AF_UNIX*/, 1 /*SOCK_STREAM*/, 0);
    if (fd < 3) {
        mark_fail("socket(AF_UNIX) failed");
        return;
    }

    // Pack a sockaddr_un with sun_path[0] = 0 to trigger the
    // EOPNOTSUPP branch in bind(). The path bytes follow the NUL;
    // they don't matter for the rejection.
    char sa[110];
    for (int i = 0; i < 110; i++) sa[i] = 0;
    // sun_family = AF_UNIX (1) in little-endian.
    sa[0] = 1;
    sa[1] = 0;
    // sun_path[0] = 0 — abstract namespace.
    // No-op; the array is already zero-filled.

    int64_t rc = sc3(NR_BIND, (uint32_t)fd, (int64_t)(intptr_t)sa, 110);
    if (rc != -95 /*EOPNOTSUPP*/) {
        mark_fail("bind(abstract) should return EOPNOTSUPP");
        return;
    }

    sc1(NR_CLOSE, (uint32_t)fd);
    mark_pass();
}
