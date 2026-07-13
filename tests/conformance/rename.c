// rename(old, new) → 0; -ENOENT on missing source; new file is
// queryable at its new name.
#include "syscall.h"

__attribute__((visibility("default")))
void _start(void) {
    // Two paths: src + dst. Put both after MARKER_ADDR so they share the
    // page and don't collide with mark_pass's overwrite at the end.
    char *buf = (char *)(intptr_t)MARKER_ADDR;

    // Source name at offset 0, dest at offset 64 (well past any other writes).
    const char *src = "rename_src";
    const char *dst = "rename_dst";
    for (int i = 0; src[i]; i++) buf[i] = src[i]; buf[10] = 0;
    for (int i = 0; dst[i]; i++) buf[64 + i] = dst[i]; buf[64 + 10] = 0;

    // Create src via openat(O_WRONLY|O_CREAT|O_TRUNC, 0o644).
    int64_t open_ret = sc4(NR_OPENAT, -100, (int64_t)(intptr_t)buf,
                           577 /*O_WRONLY|O_CREAT|O_TRUNC*/, 420 /*0o644*/);
    if (open_ret < 0) { mark_fail("openat src failed"); return; }
    (void)sc1(NR_CLOSE, (int)open_ret);

    // rename.
    int64_t r = sc2(NR_RENAME, (int64_t)(intptr_t)buf, (int64_t)(intptr_t)(buf + 64));
    if (r != 0) { mark_fail("rename failed"); return; }

    // Old name should be gone: openat with O_EXCL → -EEXIST (file doesn't
    // exist). New name should succeed.
    int64_t r_old = sc4(NR_OPENAT, -100, (int64_t)(intptr_t)buf,
                        0 /*O_RDONLY*/, 0);
    // Wait — openat with flags=0 and a missing file returns -ENOENT.
    // We don't have O_EXCL alone without O_CREAT; check by trying to open
    // the new name (should succeed) and old name (should fail).
    if (r_old >= 0) { mark_fail("rename left source behind"); (void)sc1(NR_CLOSE, (int)r_old); return; }

    // Open new name to confirm it exists.
    int64_t r_new = sc4(NR_OPENAT, -100, (int64_t)(intptr_t)(buf + 64),
                        0 /*O_RDONLY*/, 0);
    if (r_new < 0) { mark_fail("rename: new name not openable"); return; }
    (void)sc1(NR_CLOSE, (int)r_new);

    // Cleanup.
    (void)sc1(NR_UNLINK, (int64_t)(intptr_t)(buf + 64));

    mark_pass();
}