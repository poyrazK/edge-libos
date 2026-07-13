// rt_sigreturn() → 0.
#include "syscall.h"

__attribute__((visibility("default")))
void _start(void) {
    int64_t r = sc1(NR_RT_SIGRETURN, 0);
    if (r != 0) { mark_fail("rt_sigreturn != 0"); return; }
    mark_pass();
}