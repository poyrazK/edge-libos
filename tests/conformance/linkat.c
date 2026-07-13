// linkat(olddirfd, oldpath, newdirfd, newpath, flags) — hard link.
#include "syscall.h"

__attribute__((visibility("default")))
void _start(void) {
    char *buf = (char *)(intptr_t)MARKER_ADDR;
    const char *src = "la_src";
    const char *dst = "la_dst";
    for (int i = 0; src[i]; i++) buf[i] = src[i]; buf[6] = 0;
    for (int i = 0; dst[i]; i++) buf[64 + i] = dst[i]; buf[64 + 6] = 0;

    int64_t open_ret = sc4(NR_OPENAT, -100, (int64_t)(intptr_t)buf, 577, 420);
    if (open_ret < 0) { mark_fail("openat src"); return; }
    (void)sc1(NR_CLOSE, (int)open_ret);

    int64_t r = sc5(NR_LINKAT, -100, (int64_t)(intptr_t)buf,
                    -100, (int64_t)(intptr_t)(buf + 64), 0);
    if (r != 0) { mark_fail("linkat failed"); return; }

    int64_t r2 = sc4(NR_OPENAT, -100, (int64_t)(intptr_t)(buf + 64), 0, 0);
    if (r2 < 0) { mark_fail("openat dst after link"); return; }
    (void)sc1(NR_CLOSE, (int)r2);

    (void)sc1(NR_UNLINK, (int64_t)(intptr_t)buf);
    (void)sc1(NR_UNLINK, (int64_t)(intptr_t)(buf + 64));
    mark_pass();
}