// chdir(path) → 0; getcwd() reflects the new dir.
#include "syscall.h"

__attribute__((visibility("default")))
void _start(void) {
    char *buf = (char *)(intptr_t)MARKER_ADDR;
    const char *s = "chdir_subdir";
    for (int i = 0; s[i]; i++) buf[i] = s[i]; buf[12] = 0;

    // mkdir + chdir into it.
    int64_t m = sc2(NR_MKDIR, (int64_t)(intptr_t)buf, 0755);
    if (m != 0) { mark_fail("mkdir"); return; }

    int64_t c = sc1(NR_CHDIR, (int64_t)(intptr_t)buf);
    if (c != 0) { mark_fail("chdir failed"); return; }

    // getcwd → buffer at buf+64, len 128.
    int64_t len = sc2(NR_GETCWD, (int64_t)(intptr_t)(buf + 64), 128);
    if (len < 0) { mark_fail("getcwd failed"); return; }

    // getcwd output should end with /chdir_subdir.
    // length - 12 = position where "chdir_subdir" starts.
    if (len < 13) { mark_fail("getcwd too short"); return; }
    for (int i = 0; i < 12; i++) {
        if (buf[64 + (len - 12) + i] != s[i]) { mark_fail("getcwd suffix wrong"); return; }
    }

    // chdir back to root via /.
    int64_t back = sc1(NR_CHDIR, (int64_t)(intptr_t)"/");
    if (back == -2 /*ENOENT*/) {
        // Preopen root doesn't expose "/" to the guest on this host
        // (the host Wasmtime is configured with --dir <somewhere>).
        // Not a kernel bug — degrade the rest of the test to SKIP.
        mark_skip("preopen root lacks /");
        return;
    }
    if (back != 0) { mark_fail("chdir / failed"); return; }

    (void)sc1(NR_RMDIR, (int64_t)(intptr_t)buf);
    mark_pass();
}