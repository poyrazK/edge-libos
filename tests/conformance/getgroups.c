// getgroups(0, NULL) → 0; getgroups(16, list) → 0.
#include "syscall.h"

__attribute__((visibility("default")))
void _start(void) {
    int64_t r1 = sc2(NR_GETGROUPS, 0, 0);
    if (r1 != 0) { mark_fail("getgroups(0) != 0"); return; }
    char *buf = (char *)(intptr_t)MARKER_ADDR;
    int64_t r2 = sc2(NR_GETGROUPS, 16, (int64_t)(intptr_t)buf);
    if (r2 != 0) { mark_fail("getgroups(16) != 0"); return; }
    mark_pass();
}