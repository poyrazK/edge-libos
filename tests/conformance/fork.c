// fork is unimplemented in v1; assert -ENOSYS (-38).
// P3 reservation: fork() as CoW is deferred to P3 after P2-D
// snapshot machinery lands; see docs/adr/0002-snapshot-wire-format.md.
#include "syscall.h"

__attribute__((visibility("default")))
void _start(void) {
    int64_t r = sc1(NR_FORK, 0);
    if (r == -38) mark_pass();
    else mark_fail("fork != -ENOSYS");
}
