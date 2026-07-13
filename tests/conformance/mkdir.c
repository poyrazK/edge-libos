// mkdir(path, mode) → 0 on first call, -EEXIST on second.
//
// Happy path: create a directory that doesn't exist yet.
// Sad path: a second mkdir of the same path returns -EEXIST.
#include "syscall.h"

__attribute__((visibility("default")))
void _start(void) {
    // Use a unique path under the runtime's cwd.
    char *path = (char *)(intptr_t)MARKER_ADDR;
    // "mkdir_dir" — overwrite our marker prefix space; we'll mark_pass
    // at the very end after the work is done.
    const char *s = "mkdir_dir";
    for (int i = 0; s[i]; i++) path[i] = s[i];
    path[10] = 0;

    int64_t r1 = sc2(NR_MKDIR, (int64_t)(intptr_t)path, 0755);
    if (r1 != 0) { mark_fail("first mkdir failed"); return; }

    // Second mkdir should fail with -EEXIST.
    int64_t r2 = sc2(NR_MKDIR, (int64_t)(intptr_t)path, 0755);
    if (r2 != -17 /* -EEXIST */) { mark_fail("second mkdir didn't return -EEXIST"); return; }

    // Cleanup so repeated runs don't accumulate.
    (void)sc1(NR_RMDIR, (int64_t)(intptr_t)path);

    mark_pass();
}