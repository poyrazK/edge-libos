// sched_yield() → 0.
#include "syscall.h"

__attribute__((visibility("default")))
void _start(void) {
    int64_t r = sc1(NR_SCHED_YIELD, 0);
    if (r != 0) { mark_fail("sched_yield != 0"); return; }
    mark_pass();
}