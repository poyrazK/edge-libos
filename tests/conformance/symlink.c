// symlink(target, linkpath) → 0; readlink back returns target.
#include "syscall.h"

__attribute__((visibility("default")))
void _start(void) {
    char *buf = (char *)(intptr_t)MARKER_ADDR;
    const char *tgt = "sym_dest";
    const char *lnk = "sym_link";
    for (int i = 0; tgt[i]; i++) buf[i] = tgt[i]; buf[8] = 0;
    for (int i = 0; lnk[i]; i++) buf[64 + i] = lnk[i]; buf[64 + 8] = 0;

    int64_t s = sc2(NR_SYMLINK, (int64_t)(intptr_t)buf, (int64_t)(intptr_t)(buf + 64));
    if (s != 0) { mark_fail("symlink failed"); return; }

    // Now stat the link → lstat it to confirm it's a symlink by trying
    // readlink and checking the result.
    char *out = buf + 128;
    int64_t r = sc3(NR_READLINK, (int64_t)(intptr_t)(buf + 64), (int64_t)(intptr_t)out, 64);
    if (r != 8) { mark_fail("readlink back != 8"); return; }
    for (int i = 0; i < 8; i++) {
        if (out[i] != tgt[i]) { mark_fail("readlink contents wrong"); return; }
    }

    (void)sc1(NR_UNLINK, (int64_t)(intptr_t)(buf + 64));
    mark_pass();
}