// getuid: assert returns 1000.
#include "syscall.h"

__attribute__((visibility("default")))
void _start(void) {
    int64_t uid = sc1(NR_GETUID, 0);
    if (uid == 1000) mark_pass();
    else mark_fail("getuid != 1000");
}