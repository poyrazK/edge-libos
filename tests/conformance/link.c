// link(old, new) → 0; both paths refer to the same inode.
#include "syscall.h"

__attribute__((visibility("default")))
void _start(void) {
    char *buf = (char *)(intptr_t)MARKER_ADDR;
    const char *src = "ln_src";
    const char *dst = "ln_dst";
    for (int i = 0; src[i]; i++) buf[i] = src[i]; buf[6] = 0;
    for (int i = 0; dst[i]; i++) buf[64 + i] = dst[i]; buf[64 + 6] = 0;

    // Create src.
    int64_t open_ret = sc4(NR_OPENAT, -100, (int64_t)(intptr_t)buf, 577, 420);
    if (open_ret < 0) { mark_fail("openat src"); return; }
    (void)sc1(NR_CLOSE, (int)open_ret);

    // link.
    int64_t r = sc2(NR_LINK, (int64_t)(intptr_t)buf, (int64_t)(intptr_t)(buf + 64));
    if (r != 0) { mark_fail("link failed"); return; }

    // Both names should open successfully.
    int64_t r1 = sc4(NR_OPENAT, -100, (int64_t)(intptr_t)buf, 0, 0);
    if (r1 < 0) { mark_fail("open src after link"); return; }
    (void)sc1(NR_CLOSE, (int)r1);
    int64_t r2 = sc4(NR_OPENAT, -100, (int64_t)(intptr_t)(buf + 64), 0, 0);
    if (r2 < 0) { mark_fail("open dst after link"); return; }
    (void)sc1(NR_CLOSE, (int)r2);

    (void)sc1(NR_UNLINK, (int64_t)(intptr_t)buf);
    (void)sc1(NR_UNLINK, (int64_t)(intptr_t)(buf + 64));
    mark_pass();
}