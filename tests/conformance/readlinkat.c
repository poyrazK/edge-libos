// readlinkat(dirfd, path, buf, len) — same as readlink but with dirfd.
#include "syscall.h"

__attribute__((visibility("default")))
void _start(void) {
    char *buf = (char *)(intptr_t)MARKER_ADDR;
    const char *tgt = "rla_tgt";
    const char *lnk = "rla_lnk";
    for (int i = 0; tgt[i]; i++) buf[i] = tgt[i]; buf[7] = 0;
    for (int i = 0; lnk[i]; i++) buf[64 + i] = lnk[i]; buf[64 + 7] = 0;

    // Self-cleanup of a leftover symlink from a prior run that didn't
    // reach its unlink. NOOP if absent.
    (void)sc1(NR_UNLINK, (int64_t)(intptr_t)(buf + 64));

    int64_t s = sc2(NR_SYMLINK, (int64_t)(intptr_t)buf, (int64_t)(intptr_t)(buf + 64));
    if (s != 0) { mark_fail("symlink"); return; }

    char *out = buf + 128;
    int64_t r = sc4(NR_READLINKAT, -100, (int64_t)(intptr_t)(buf + 64),
                    (int64_t)(intptr_t)out, 64);
    if (r != 7) { mark_fail("readlinkat length != 7"); return; }

    (void)sc1(NR_UNLINK, (int64_t)(intptr_t)(buf + 64));
    mark_pass();
}