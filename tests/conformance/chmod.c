// chmod(path, mode) — change permissions; faccessat(R_OK) reflects them.
#include "syscall.h"

__attribute__((visibility("default")))
void _start(void) {
    char *buf = (char *)(intptr_t)MARKER_ADDR;
    const char *s = "chmod_file";
    for (int i = 0; s[i]; i++) buf[i] = s[i]; buf[10] = 0;

    // Create via openat(O_WRONLY|O_CREAT|O_EXCL).
    int64_t open_ret = sc4(NR_OPENAT, -100, (int64_t)(intptr_t)buf, 193, 420);
    if (open_ret == -17 /*-EEXIST*/) {
        mark_skip("chmod_file leftover from prior run");
        return;
    }
    if (open_ret < 0) { mark_fail("openat"); return; }
    (void)sc1(NR_CLOSE, (int)open_ret);

    // chmod 0o644 (= 420 + 064 in standard shell notation, decimal 420 = 0644).
    int64_t r = sc2(NR_CHMOD, (int64_t)(intptr_t)buf, 420);
    if (r != 0) { mark_fail("chmod 0644 failed"); return; }

    // chmod 0o000 — file becomes read-only. faccessat(W_OK) should fail.
    int64_t r2 = sc2(NR_CHMOD, (int64_t)(intptr_t)buf, 0);
    if (r2 != 0) { mark_fail("chmod 0 failed"); return; }

    int64_t r3 = sc4(NR_FACCESSAT, -100, (int64_t)(intptr_t)buf, W_OK, 0);
    if (r3 != -13 /* -EACCES */) { mark_fail("faccessat W_OK after 0 != -EACCES"); return; }

    // Restore so we can unlink.
    (void)sc2(NR_CHMOD, (int64_t)(intptr_t)buf, 420);
    (void)sc1(NR_UNLINK, (int64_t)(intptr_t)buf);
    mark_pass();
}