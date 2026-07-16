// fchmod(fd, mode) — change permissions via fd.
#include "syscall.h"

__attribute__((visibility("default")))
void _start(void) {
    char *buf = (char *)(intptr_t)MARKER_ADDR;
    const char *s = "fchmod_file";
    for (int i = 0; s[i]; i++) buf[i] = s[i]; buf[11] = 0;

    int64_t open_ret = sc4(NR_OPENAT, -100, (int64_t)(intptr_t)buf, 193, 420);
    if (open_ret == -17 /*-EEXIST*/) {
        mark_skip("fchmod_file leftover from prior run");
        return;
    }
    if (open_ret < 0) { mark_fail("openat"); return; }
    int fd = (int)open_ret;

    int64_t r = sc2(NR_FCHMOD, fd, 292 /* 0o444 */);
    if (r != 0) { mark_fail("fchmod 0o444 failed"); return; }

    int64_t r2 = sc4(NR_FACCESSAT, -100, (int64_t)(intptr_t)buf, W_OK, 0);
    if (r2 != -13) { mark_fail("W_OK after 0o444 != -EACCES"); return; }

    (void)sc2(NR_FCHMOD, fd, 420);
    (void)sc1(NR_CLOSE, fd);
    (void)sc1(NR_UNLINK, (int64_t)(intptr_t)buf);
    mark_pass();
}