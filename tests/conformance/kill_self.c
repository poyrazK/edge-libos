// kill(0, 0) → 0 (self-only in v1).
#include "syscall.h"

__attribute__((visibility("default")))
void _start(void) {
    int64_t r = sc2(NR_KILL, 0, 0);
    if (r != 0) { mark_fail("kill(0,0) failed"); return; }
    // Non-self → -ESRCH
    int64_t r2 = sc2(NR_KILL, 999, 0);
    if (r2 != -3) { mark_fail("kill(999) != -ESRCH"); return; }
    mark_pass();
}