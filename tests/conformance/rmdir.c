// rmdir(path) → 0; -ENOENT on a missing path; -ENOTEMPTY on a non-empty dir.
#include "syscall.h"

__attribute__((visibility("default")))
void _start(void) {
    char *path = (char *)(intptr_t)MARKER_ADDR;
    const char *s = "rmdir_dir";
    for (int i = 0; s[i]; i++) path[i] = s[i];
    path[9] = 0;

    // Create it.
    int64_t mk = sc2(NR_MKDIR, (int64_t)(intptr_t)path, 0755);
    if (mk != 0) { mark_fail("mkdir setup"); return; }

    // Remove it.
    int64_t r = sc1(NR_RMDIR, (int64_t)(intptr_t)path);
    if (r != 0) { mark_fail("rmdir failed"); return; }

    // Second rmdir of the same path → -ENOENT.
    int64_t r2 = sc1(NR_RMDIR, (int64_t)(intptr_t)path);
    if (r2 != -2 /* -ENOENT */) { mark_fail("second rmdir didn't return -ENOENT"); return; }

    mark_pass();
}