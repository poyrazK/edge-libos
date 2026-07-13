// prlimit64(0, RLIMIT_STACK, NULL, &old) → 0; old = infinity.
#include "syscall.h"

__attribute__((visibility("default")))
void _start(void) {
    char *buf = (char *)(intptr_t)MARKER_ADDR;
    int64_t r = sc4(NR_PRLIMIT64, 0, 3 /*RLIMIT_STACK*/, 0, (int64_t)(intptr_t)buf);
    if (r != 0) { mark_fail("prlimit64 failed"); return; }
    // rlim_cur should be u64::MAX.
    int64_t cur = 0;
    for (int i = 0; i < 8; i++) {
        cur |= ((int64_t)(unsigned char)buf[i]) << (8 * i);
    }
    if (cur != -1) { mark_fail("rlim_cur != infinity"); return; }
    mark_pass();
}