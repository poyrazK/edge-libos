// readv(fd, iov, 2): writes 6 bytes ("abcdef") to /rv via open+write+
// lseek, then readv scatters them into two 3-byte buffers.
#include "syscall.h"
#include <stdint.h>

// zig cc targeting wasm32-freestanding does not expose <fcntl.h>; locally
// define the O_* flags we need (same convention as openat_close.c).
#ifndef O_WRONLY
#define O_WRONLY 1
#endif
#ifndef O_CREAT
#define O_CREAT 0100
#endif
#ifndef O_TRUNC
#define O_TRUNC 01000
#endif

__attribute__((visibility("default")))
void _start(void) {
    char *path = (char *)(intptr_t)(MARKER_ADDR + 1024);
    path[0] = '/'; path[1] = 'r'; path[2] = 'v'; path[3] = 0;

    // Create /rv with 6 bytes of content.
    int64_t fd = sc3(NR_OPEN, (int64_t)(intptr_t)path,
                     O_WRONLY | O_CREAT | O_TRUNC, 0666);
    if (fd < 3) { mark_fail("open(O_CREAT) failed"); return; }

    char *src = (char *)(intptr_t)(MARKER_ADDR + 1100);
    src[0] = 'a'; src[1] = 'b'; src[2] = 'c';
    src[3] = 'd'; src[4] = 'e'; src[5] = 'f';
    int64_t w = sc3(NR_WRITE, fd, (int64_t)(intptr_t)src, 6);
    if (w != 6) { mark_fail("write != 6"); return; }

    // Seek to 0 so readv starts from the beginning.
    sc3(NR_LSEEK, fd, 0, 0 /*SEEK_SET*/);

    // iovec at MARKER_ADDR + 1200: two 3-byte buffers.
    uint32_t *iov = (uint32_t *)(intptr_t)(MARKER_ADDR + 1200);
    iov[0] = MARKER_ADDR + 1300;  iov[1] = 3;
    iov[2] = MARKER_ADDR + 1400;  iov[3] = 3;

    int64_t n = sc3(NR_READV, fd, (int64_t)(intptr_t)iov, 2);
    if (n != 6) { mark_fail("readv != 6"); return; }
    char *b1 = (char *)(intptr_t)(MARKER_ADDR + 1300);
    char *b2 = (char *)(intptr_t)(MARKER_ADDR + 1400);
    if (b1[0] == 'a' && b1[1] == 'b' && b1[2] == 'c' &&
        b2[0] == 'd' && b2[1] == 'e' && b2[2] == 'f') {
        mark_pass();
    } else {
        mark_fail("readv buffers wrong");
    }
}
