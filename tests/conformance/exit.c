// exit(0) succeeds — the host driver propagates the code.
#include "syscall.h"

__attribute__((visibility("default")))
void _start(void) {
    sc1(NR_EXIT, 0);
    // Never reached.
    mark_fail("returned from exit");
}