// clone is unimplemented in v1; assert -ENOSYS (-38).
// P3 reservation: see docs/adr/0001-p3-futex-semantics.md.
#include "syscall.h"

__attribute__((visibility("default")))
void _start(void) {
    int64_t r = sc6(NR_CLONE, 0, 0, 0, 0, 0, 0);
    if (r == -38) mark_pass();
    else mark_fail("clone != -ENOSYS");
}
