// brk(0) returns current program break (>= 0).
#include "syscall.h"

__attribute__((visibility("default")))
void _start(void) {
    int64_t brk = sc1(NR_BRK, 0);
    if (brk >= 0) mark_pass();
    else mark_fail("brk < 0");
}