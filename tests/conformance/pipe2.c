// pipe2(fdarray, 0): writes 2 u32 fds into fdarray[0..8]. Verify both
// are >= 3 and distinct.
#include "syscall.h"

__attribute__((visibility("default")))
void _start(void) {
    uint32_t fds[2] = {0, 0};
    int64_t r = sc2(NR_PIPE2, (int64_t)(intptr_t)fds, 0);
    if (r != 0) { mark_fail("pipe2 != 0"); return; }
    if (fds[0] >= 3 && fds[1] >= 3 && fds[0] != fds[1]) mark_pass();
    else mark_fail("pipe2 returned invalid fds");
}