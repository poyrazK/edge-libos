// fcntl(rd, F_DUPFD, MIN) >= MIN.
//
// Regression guard for the P2-B5 fix: F_DUPFD previously ignored the
// minimum-fd arg (fcntl arm used fds.insert(cloned) unconditionally,
// landing at next_fd regardless of arg). After the fix, the new fd
// is inserted at or above MIN. We pick MIN = 100 to make a correct
// implementation obvious and a buggy one distinct (lowest-free).
#include "syscall.h"

#define MIN 100

__attribute__((visibility("default")))
void _start(void) {
    int32_t *fds_arr = (int32_t *)(intptr_t)MARKER_ADDR;
    int64_t pipe2_ret = sc2(NR_PIPE2, (int64_t)(intptr_t)fds_arr, 0);
    if (pipe2_ret != 0) { mark_fail("pipe2 failed"); return; }
    int rd = (int)fds_arr[0];

    // F_DUPFD(rd, MIN). Arg layout per Linux: cmd=F_DUPFD, arg=MIN.
    int64_t r = sc3(NR_FCNTL, rd, F_DUPFD, MIN);
    if (r < 0) { mark_fail("fcntl F_DUPFD returned errno"); return; }
    int new_fd = (int)r;
    if (new_fd < MIN) { mark_fail("F_DUPFD ignored minimum-fd"); return; }

    mark_pass();
}
