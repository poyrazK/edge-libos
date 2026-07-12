// rseq is unimplemented; assert -ENOSYS (-38).
#include "syscall.h"

__attribute__((visibility("default")))
void _start(void) {
    int64_t r = sc6(NR_RSEQ, 0, 0, 0, 0, 0, 0);
    if (r == -38) mark_pass();
    else mark_fail("rseq != -ENOSYS");
}