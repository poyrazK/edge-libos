// readlink(path, buf, buf_len) → byte count or -ENOENT.
//
// symlink("rl_target", "rl_link"); readlink("rl_link", ...).
#include "syscall.h"

__attribute__((visibility("default")))
void _start(void) {
    char *buf = (char *)(intptr_t)MARKER_ADDR;
    // Two paths: target at offset 0, link at offset 64.
    const char *tgt = "rl_target";
    const char *lnk = "rl_link";
    for (int i = 0; tgt[i]; i++) buf[i] = tgt[i]; buf[9] = 0;
    for (int i = 0; lnk[i]; i++) buf[64 + i] = lnk[i]; buf[64 + 7] = 0;

    // Output buffer: 64 bytes at offset 128.
    char *out = buf + 128;

    // Self-cleanup: drop any leftover symlink from a prior run that
    // didn't reach its unlink. NOOP if absent.
    (void)sc1(NR_UNLINK, (int64_t)(intptr_t)(buf + 64));
    (void)sc1(NR_UNLINK, (int64_t)(intptr_t)buf);

    // Create the symlink.
    int64_t s = sc2(NR_SYMLINK, (int64_t)(intptr_t)buf, (int64_t)(intptr_t)(buf + 64));
    if (s != 0) { mark_fail("symlink failed"); return; }

    // readlink into out (64 bytes available).
    int64_t r = sc3(NR_READLINK, (int64_t)(intptr_t)(buf + 64),
                    (int64_t)(intptr_t)out, 64);
    if (r != 9) { mark_fail("readlink length != 9"); return; }
    // Verify contents.
    for (int i = 0; i < 9; i++) {
        if (out[i] != tgt[i]) { mark_fail("readlink contents wrong"); return; }
    }

    // Cleanup.
    (void)sc1(NR_UNLINK, (int64_t)(intptr_t)(buf + 64));
    (void)sc1(NR_UNLINK, (int64_t)(intptr_t)buf);

    mark_pass();
}