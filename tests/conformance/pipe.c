// pipe(fdarray): legacy shim — same contract as pipe2 with flags=0.
#include "syscall.h"

__attribute__((visibility("default")))
void _start(void) {
    uint32_t fds[2] = {0, 0};
    int64_t r = sc1(NR_PIPE, (int64_t)(intptr_t)fds);
    if (r != 0) { mark_fail("pipe != 0"); return; }
    if (fds[0] >= 3 && fds[1] >= 3 && fds[0] != fds[1]) mark_pass();
    else mark_fail("pipe returned invalid fds");
}
