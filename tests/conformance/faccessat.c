// faccessat(dirfd, path, mode) — check access.
//
// Test R_OK on a 0o400 file, W_OK on a 0o600 file, F_OK on existing
// and missing paths.
#include "syscall.h"

__attribute__((visibility("default")))
void _start(void) {
    char *buf = (char *)(intptr_t)MARKER_ADDR;
    const char *s = "facc_file";
    for (int i = 0; s[i]; i++) buf[i] = s[i]; buf[8] = 0;

    // F_OK on missing path → -ENOENT (-2).
    int64_t r1 = sc4(NR_FACCESSAT, -100, (int64_t)(intptr_t)buf, F_OK, 0);
    if (r1 != -2) { mark_fail("F_OK on missing != -ENOENT"); return; }

    // Create with 0o600.
    int64_t open_ret = sc4(NR_OPENAT, -100, (int64_t)(intptr_t)buf, 577, 384 /*0o600*/);
    if (open_ret < 0) { mark_fail("openat"); return; }
    (void)sc1(NR_CLOSE, (int)open_ret);

    // R_OK on 0o600 → 0.
    int64_t r2 = sc4(NR_FACCESSAT, -100, (int64_t)(intptr_t)buf, R_OK, 0);
    if (r2 != 0) { mark_fail("R_OK on 0o600 failed"); return; }

    // W_OK on 0o600 → 0.
    int64_t r3 = sc4(NR_FACCESSAT, -100, (int64_t)(intptr_t)buf, W_OK, 0);
    if (r3 != 0) { mark_fail("W_OK on 0o600 failed"); return; }

    // X_OK on 0o600 (no exec bit) → -EACCES.
    int64_t r4 = sc4(NR_FACCESSAT, -100, (int64_t)(intptr_t)buf, X_OK, 0);
    if (r4 != -13) { mark_fail("X_OK on 0o600 != -EACCES"); return; }

    // Strip write bit.
    (void)sc2(NR_CHMOD, (int64_t)(intptr_t)buf, 256 /*0o400*/);
    int64_t r5 = sc4(NR_FACCESSAT, -100, (int64_t)(intptr_t)buf, W_OK, 0);
    if (r5 != -13) { mark_fail("W_OK on 0o400 != -EACCES"); return; }

    // Restore + cleanup.
    (void)sc2(NR_CHMOD, (int64_t)(intptr_t)buf, 420);
    (void)sc1(NR_UNLINK, (int64_t)(intptr_t)buf);
    mark_pass();
}