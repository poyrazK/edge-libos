// unlink(path) → 0; -ENOENT on a missing path; -EISDIR on a directory.
#include "syscall.h"

__attribute__((visibility("default")))
void _start(void) {
    // Use a unique path under the runtime's cwd.
    char *path = (char *)(intptr_t)MARKER_ADDR;
    const char *s = "unlink_file";
    for (int i = 0; s[i]; i++) path[i] = s[i];
    path[11] = 0;

    // Create the file via openat. O_WRONLY|O_CREAT|O_TRUNC is idempotent
    // across runs (truncates if present, creates if absent).
    // Flags are decimal: 1 | 64 | 512 = 577. Mode 0o644 = 420.
    int64_t open_ret = sc4(NR_OPENAT, -100 /*AT_FDCWD*/, (int64_t)(intptr_t)path,
                           577 /*O_WRONLY|O_CREAT|O_TRUNC*/, 420 /*0o644*/);
    if (open_ret < 0) { mark_fail("openat setup failed"); return; }
    int fd = (int)open_ret;
    (void)sc1(NR_CLOSE, fd);

    // unlink it.
    int64_t r = sc1(NR_UNLINK, (int64_t)(intptr_t)path);
    if (r != 0) { mark_fail("unlink failed"); return; }

    // Second unlink → -ENOENT.
    int64_t r2 = sc1(NR_UNLINK, (int64_t)(intptr_t)path);
    if (r2 != -2 /* -ENOENT */) { mark_fail("second unlink didn't return -ENOENT"); return; }

    mark_pass();
}