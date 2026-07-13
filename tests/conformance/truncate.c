// truncate(path, len) → 0; sets file size via path.
#include "syscall.h"

__attribute__((visibility("default")))
void _start(void) {
    char *buf = (char *)(intptr_t)MARKER_ADDR;
    const char *s = "truncate_file";
    for (int i = 0; s[i]; i++) buf[i] = s[i]; buf[13] = 0;

    // truncate on a non-existing path: should still succeed (OpenOptions::create).
    int64_t r1 = sc2(NR_TRUNCATE, (int64_t)(intptr_t)buf, 32);
    if (r1 != 0) { mark_fail("truncate create failed"); return; }

    // Verify by opening + lseek to end + reading the position.
    int64_t open_ret = sc4(NR_OPENAT, -100, (int64_t)(intptr_t)buf, 0 /*O_RDONLY*/, 0);
    if (open_ret < 0) { mark_fail("openat for verify failed"); return; }
    int fd = (int)open_ret;

    // lseek(fd, 0, SEEK_END) → file size = 32.
    int64_t size = sc3(NR_LSEEK, fd, 0, 2 /*SEEK_END*/);
    if (size != 32) { mark_fail("truncate size != 32"); (void)sc1(NR_CLOSE, fd); (void)sc1(NR_UNLINK, (int64_t)(intptr_t)buf); return; }

    (void)sc1(NR_CLOSE, fd);
    (void)sc1(NR_UNLINK, (int64_t)(intptr_t)buf);

    mark_pass();
}