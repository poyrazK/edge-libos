// renameat2 with RENAME_NOREPLACE → -EEXIST if dest already exists.
//
// Sets up src + dst files, then attempts to rename src → dst with the
// RENAME_NOREPLACE flag. Expects -EEXIST (= -17).
#include "syscall.h"

#define RENAME_NOREPLACE 1

__attribute__((visibility("default")))
void _start(void) {
    char *buf = (char *)(intptr_t)MARKER_ADDR;
    const char *src = "rn2_src";
    const char *dst = "rn2_dst";
    for (int i = 0; src[i]; i++) buf[i] = src[i]; buf[7] = 0;
    for (int i = 0; dst[i]; i++) buf[64 + i] = dst[i]; buf[64 + 7] = 0;

    // Create both src and dst via openat(O_WRONLY|O_CREAT|O_EXCL).
    // EXCL (vs O_TRUNC) makes leftover fixtures from prior runs
    // surface as -EEXIST and skip the rest of the test.
    int64_t open_ret = sc4(NR_OPENAT, -100, (int64_t)(intptr_t)buf,
                           193 /*O_WRONLY|O_CREAT|O_EXCL*/, 420);
    if (open_ret == -17 /*-EEXIST*/) {
        mark_skip("rn2_src leftover from prior run");
        return;
    }
    if (open_ret < 0) { mark_fail("openat src"); return; }
    (void)sc1(NR_CLOSE, (int)open_ret);

    open_ret = sc4(NR_OPENAT, -100, (int64_t)(intptr_t)(buf + 64),
                   193, 420);
    if (open_ret == -17 /*-EEXIST*/) {
        // rn2_dst leftover — undo the src we just made so the next
        // run starts clean.
        (void)sc1(NR_UNLINK, (int64_t)(intptr_t)buf);
        mark_skip("rn2_dst leftover from prior run");
        return;
    }
    if (open_ret < 0) { mark_fail("openat dst"); (void)sc1(NR_UNLINK, (int64_t)(intptr_t)buf); return; }
    (void)sc1(NR_CLOSE, (int)open_ret);

    // renameat2(AT_FDCWD, src, AT_FDCWD, dst, RENAME_NOREPLACE).
    int64_t r = sc6(NR_RENAMEAT2, -100, (int64_t)(intptr_t)buf,
                    -100, (int64_t)(intptr_t)(buf + 64),
                    RENAME_NOREPLACE, 0);
    if (r != -17 /* -EEXIST */) { mark_fail("RENAME_NOREPLACE didn't return -EEXIST"); return; }

    // Now do it without the flag — should succeed.
    int64_t r2 = sc6(NR_RENAMEAT2, -100, (int64_t)(intptr_t)buf,
                     -100, (int64_t)(intptr_t)(buf + 64),
                     0, 0);
    if (r2 != 0) { mark_fail("plain renameat2 failed"); return; }

    // Cleanup.
    (void)sc1(NR_UNLINK, (int64_t)(intptr_t)(buf + 64));

    mark_pass();
}