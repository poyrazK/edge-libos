// set_tid_address: returns the calling tid (1 in P0).
#include "syscall.h"

__attribute__((visibility("default")))
void _start(void) {
    int64_t tid = sc1(NR_SET_TID_ADDRESS, 0);
    if (tid > 0) mark_pass();
    else mark_fail("set_tid_address returned non-positive");
}