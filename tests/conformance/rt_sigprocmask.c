// rt_sigprocmask(SIG_BLOCK, NULL, &old, 8): writes 8 bytes; returns 0.
#include "syscall.h"

#define SIG_BLOCK 0
#define SIG_SETMASK 2

__attribute__((visibility("default")))
void _start(void) {
    uint64_t old = 0xdead;
    int64_t r = sc4(NR_RT_SIGPROCMASK, SIG_BLOCK, 0, (int64_t)(intptr_t)&old, 8);
    if (r == 0) mark_pass();
    else mark_fail("rt_sigprocmask != 0");
}