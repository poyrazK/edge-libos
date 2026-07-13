// tgkill(0, 0, 0) → 0 (self).
#include "syscall.h"

__attribute__((visibility("default")))
void _start(void) {
    int64_t r = sc3(NR_TGKILL, 0, 0, 0);
    if (r != 0) { mark_fail("tgkill self failed"); return; }
    int64_t r2 = sc3(NR_TGKILL, 999, 999, 0);
    if (r2 != -3) { mark_fail("tgkill non-self != -ESRCH"); return; }
    mark_pass();
}