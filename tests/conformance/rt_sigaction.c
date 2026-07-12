// rt_sigaction with NULL act and oldact: returns 0.
#include "syscall.h"

__attribute__((visibility("default")))
void _start(void) {
    int64_t r = sc4(NR_RT_SIGACTION, 2 /*SIGINT*/, 0, 0, 8);
    if (r == 0) mark_pass();
    else mark_fail("rt_sigaction != 0");
}