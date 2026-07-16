// faccessat2(dirfd, path, mode, flags) — same as faccessat for our flags.
#include "syscall.h"

__attribute__((visibility("default")))
void _start(void) {
    char *buf = (char *)(intptr_t)MARKER_ADDR;
    const char *s = "fa2_file";
    for (int i = 0; s[i]; i++) buf[i] = s[i]; buf[8] = 0;

    int64_t open_ret = sc4(NR_OPENAT, -100, (int64_t)(intptr_t)buf, 193, 384);
    if (open_ret == -17 /*-EEXIST*/) {
        mark_skip("fa2_file leftover from prior run");
        return;
    }
    if (open_ret < 0) { mark_fail("openat"); return; }
    (void)sc1(NR_CLOSE, (int)open_ret);

    int64_t r1 = sc4(NR_FACCESSAT2, -100, (int64_t)(intptr_t)buf, R_OK, 0);
    if (r1 != 0) { mark_fail("R_OK != 0"); return; }
    int64_t r2 = sc4(NR_FACCESSAT2, -100, (int64_t)(intptr_t)buf, W_OK, 0);
    if (r2 != 0) { mark_fail("W_OK != 0"); return; }
    int64_t r3 = sc4(NR_FACCESSAT2, -100, (int64_t)(intptr_t)buf, X_OK, 0);
    if (r3 != -13) { mark_fail("X_OK != -EACCES"); return; }
    int64_t r4 = sc4(NR_FACCESSAT2, -100, (int64_t)(intptr_t)buf, F_OK, 0);
    if (r4 != 0) { mark_fail("F_OK != 0"); return; }

    (void)sc1(NR_UNLINK, (int64_t)(intptr_t)buf);
    mark_pass();
}