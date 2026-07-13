// getsid(0) → 1; setsid() → -EPERM.
#include "syscall.h"

__attribute__((visibility("default")))
void _start(void) {
    int64_t r1 = sc1(NR_GETSID, 0);
    if (r1 != 1) { mark_fail("getsid != 1"); return; }
    int64_t r2 = sc1(NR_SETSID, 0);
    if (r2 != -1) { mark_fail("setsid != -EPERM"); return; }
    mark_pass();
}