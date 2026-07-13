// wait4 is unimplemented in v1; assert -ENOSYS (-38).
// P3 reservation: pairs with fork(); see docs/adr/0002-snapshot-wire-format.md.
#include "syscall.h"

__attribute__((visibility("default")))
void _start(void) {
    int64_t r = sc4(NR_WAIT4, 0, 0, 0, 0);
    if (r == -38) mark_pass();
    else mark_fail("wait4 != -ENOSYS");
}
