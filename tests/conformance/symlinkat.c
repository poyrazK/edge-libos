// symlinkat(target, newdirfd, linkpath) — create symlink with dirfd.
#include "syscall.h"

__attribute__((visibility("default")))
void _start(void) {
    char *buf = (char *)(intptr_t)MARKER_ADDR;
    const char *tgt = "sla_tgt";
    const char *lnk = "sla_lnk";
    for (int i = 0; tgt[i]; i++) buf[i] = tgt[i]; buf[7] = 0;
    for (int i = 0; lnk[i]; i++) buf[64 + i] = lnk[i]; buf[64 + 7] = 0;

    int64_t s = sc3(NR_SYMLINKAT, (int64_t)(intptr_t)buf, -100, (int64_t)(intptr_t)(buf + 64));
    if (s != 0) { mark_fail("symlinkat"); return; }

    char *out = buf + 128;
    int64_t r = sc3(NR_READLINK, (int64_t)(intptr_t)(buf + 64), (int64_t)(intptr_t)out, 64);
    if (r != 7) { mark_fail("readlink length != 7"); return; }

    (void)sc1(NR_UNLINK, (int64_t)(intptr_t)(buf + 64));
    mark_pass();
}