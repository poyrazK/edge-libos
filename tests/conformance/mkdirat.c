// mkdirat(AT_FDCWD, path, mode) → 0; equivalent to mkdir.
//
// Confirms the *at variant with the canonical dirfd works.
#include "syscall.h"

__attribute__((visibility("default")))
void _start(void) {
    char *path = (char *)(intptr_t)MARKER_ADDR;
    const char *s = "mkdirat_dir";
    for (int i = 0; s[i]; i++) path[i] = s[i];
    path[11] = 0;

    int64_t r1 = sc3(NR_MKDIRAT, -100 /*AT_FDCWD*/, (int64_t)(intptr_t)path, 0755);
    if (r1 != 0) { mark_fail("first mkdirat failed"); return; }

    // Same path again → -EEXIST.
    int64_t r2 = sc3(NR_MKDIRAT, -100, (int64_t)(intptr_t)path, 0755);
    if (r2 != -17 /* -EEXIST */) { mark_fail("second mkdirat didn't return -EEXIST"); return; }

    (void)sc1(NR_RMDIR, (int64_t)(intptr_t)path);

    mark_pass();
}