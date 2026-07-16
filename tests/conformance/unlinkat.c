// unlinkat(AT_FDCWD, path, AT_REMOVEDIR) → 0 (acts like rmdir).
// unlinkat(AT_FDCWD, path, 0) → 0 (acts like unlink).
#include "syscall.h"

__attribute__((visibility("default")))
void _start(void) {
    char *path = (char *)(intptr_t)MARKER_ADDR;
    const char *s = "unlinkat_dir";
    for (int i = 0; s[i]; i++) path[i] = s[i];
    path[12] = 0;

    // Create a directory.
    int64_t mk = sc2(NR_MKDIR, (int64_t)(intptr_t)path, 0755);
    if (mk == -17 /*-EEXIST*/) {
        // unlinkat_dir leftover from a prior run. Skip the
        // REMOVEDIR contract assertion below rather than failing
        // on a fixture that's already there.
        mark_skip("unlinkat_dir leftover from prior run");
        return;
    }
    if (mk != 0) { mark_fail("mkdir setup"); return; }

    // unlinkat with AT_REMOVEDIR=0x200 → 0 (removes the dir).
    int64_t r1 = sc3(NR_UNLINKAT, -100, (int64_t)(intptr_t)path, 0x200 /*AT_REMOVEDIR*/);
    if (r1 != 0) { mark_fail("unlinkat REMOVEDIR failed"); return; }

    // Create a regular file.
    const char *s2 = "unlinkat_file";
    for (int i = 0; s2[i]; i++) path[i] = s2[i];
    path[12] = 0;
    int64_t open_ret = sc4(NR_OPENAT, -100, (int64_t)(intptr_t)path,
                           193 /*O_WRONLY|O_CREAT|O_EXCL*/, 420 /*0o644*/);
    if (open_ret == -17 /*-EEXIST*/) {
        // Same leftover class as above for the file path variant of
        // the test. Skip rather than fail.
        mark_skip("unlinkat_file leftover from prior run");
        return;
    }
    if (open_ret < 0) { mark_fail("openat setup failed"); return; }
    int fd = (int)open_ret;
    (void)sc1(NR_CLOSE, fd);

    // unlinkat with flags=0 → 0 (removes the file).
    int64_t r2 = sc3(NR_UNLINKAT, -100, (int64_t)(intptr_t)path, 0);
    if (r2 != 0) { mark_fail("unlinkat (no flag) failed"); return; }

    mark_pass();
}