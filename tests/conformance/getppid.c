// getppid() → 1 (single-process v1).
#include "syscall.h"

__attribute__((visibility("default")))
void _start(void) {
    int64_t r = sc1(NR_GETPPID, 0);
    if (r != 1) { mark_fail("getppid != 1"); return; }
    mark_pass();
}