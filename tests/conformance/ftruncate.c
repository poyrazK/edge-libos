// ftruncate(fd, len) → 0; shrinks and extends a file.
#include "syscall.h"

__attribute__((visibility("default")))
void _start(void) {
    char *buf = (char *)(intptr_t)MARKER_ADDR;
    const char *s = "ftruncate_file";
    for (int i = 0; s[i]; i++) buf[i] = s[i]; buf[14] = 0;

    // Create + write 16 bytes via openat(O_WRONLY|O_CREAT|O_EXCL).
    int64_t open_ret = sc4(NR_OPENAT, -100, (int64_t)(intptr_t)buf,
                           193 /*O_WRONLY|O_CREAT|O_EXCL*/, 420);
    if (open_ret == -17 /*-EEXIST*/) {
        mark_skip("ftruncate_file leftover from prior run");
        return;
    }
    if (open_ret < 0) { mark_fail("openat failed"); return; }
    int fd = (int)open_ret;

    // ftruncate(fd, 64) — extend to 64 bytes (zero-filled).
    int64_t r1 = sc2(NR_FTRUNCATE, fd, 64);
    if (r1 != 0) { mark_fail("ftruncate extend failed"); return; }

    // ftruncate(fd, 8) — shrink to 8 bytes.
    int64_t r2 = sc2(NR_FTRUNCATE, fd, 8);
    if (r2 != 0) { mark_fail("ftruncate shrink failed"); return; }

    // ftruncate with negative len → -EINVAL (=-22).
    int64_t r3 = sc2(NR_FTRUNCATE, fd, -1);
    if (r3 != -22) { mark_fail("ftruncate negative didn't return -EINVAL"); return; }

    (void)sc1(NR_CLOSE, fd);
    (void)sc1(NR_UNLINK, (int64_t)(intptr_t)buf);

    mark_pass();
}