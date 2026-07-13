// ioctl(fd, FIONBIO, 1) on a regular file — returns 0; clear returns 0.
// (File resources accept FIONBIO silently; this exercises the codepath.)
#include "syscall.h"

__attribute__((visibility("default")))
void _start(void) {
    char *buf = (char *)(intptr_t)MARKER_ADDR;
    const char *s = "fnb_file";
    for (int i = 0; s[i]; i++) buf[i] = s[i]; buf[8] = 0;

    // Create + open.
    int64_t fd = sc4(NR_OPENAT, -100, (int64_t)(intptr_t)buf, 577, 420);
    if (fd < 0) { mark_fail("openat failed"); return; }

    int64_t r1 = sc3(NR_IOCTL, (int)fd, FIONBIO, 1);
    if (r1 != 0) { mark_fail("FIONBIO set failed"); (void)sc1(NR_CLOSE, (int)fd); return; }
    int64_t r2 = sc3(NR_IOCTL, (int)fd, FIONBIO, 0);
    if (r2 != 0) { mark_fail("FIONBIO clear failed"); (void)sc1(NR_CLOSE, (int)fd); return; }

    (void)sc1(NR_CLOSE, (int)fd);
    (void)sc1(NR_UNLINK, (int64_t)(intptr_t)buf);
    mark_pass();
}