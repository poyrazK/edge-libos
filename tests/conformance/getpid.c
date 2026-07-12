// getpid: assert returns 1 (P0 single-process).
#include "syscall.h"

__attribute__((visibility("default")))
void _start(void) {
    int64_t pid = sc1(NR_GETPID, 0);
    if (pid == 1) mark_pass();
    else mark_fail("getpid != 1");
}