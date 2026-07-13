// fchmodat(dirfd, path, mode, flags) — chmod with dirfd.
#include "syscall.h"

__attribute__((visibility("default")))
void _start(void) {
    char *buf = (char *)(intptr_t)MARKER_ADDR;
    const char *s = "fca_file";
    for (int i = 0; s[i]; i++) buf[i] = s[i]; buf[8] = 0;

    int64_t open_ret = sc4(NR_OPENAT, -100, (int64_t)(intptr_t)buf, 577, 420);
    if (open_ret < 0) { mark_fail("openat"); return; }
    (void)sc1(NR_CLOSE, (int)open_ret);

    int64_t r = sc4(NR_FCHMODAT, -100, (int64_t)(intptr_t)buf, 0, 0);
    if (r != 0) { mark_fail("fchmodat 0 failed"); return; }

    int64_t r2 = sc4(NR_FACCESSAT, -100, (int64_t)(intptr_t)buf, W_OK, 0);
    if (r2 != -13) { mark_fail("W_OK after 0 != -EACCES"); return; }

    (void)sc4(NR_FCHMODAT, -100, (int64_t)(intptr_t)buf, 420, 0);
    (void)sc1(NR_UNLINK, (int64_t)(intptr_t)buf);
    mark_pass();
}