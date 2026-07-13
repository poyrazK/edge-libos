// sched_getaffinity(0, 8, &mask) → 8; mask[0] = 0x01 (CPU 0 only).
#include "syscall.h"

__attribute__((visibility("default")))
void _start(void) {
    char *buf = (char *)(intptr_t)MARKER_ADDR;
    int64_t r = sc3(NR_SCHED_GETAFFINITY, 0, 8, (int64_t)(intptr_t)buf);
    if (r != 8) { mark_fail("sched_getaffinity != 8"); return; }
    if ((unsigned char)buf[0] != 0x01) { mark_fail("mask[0] != 0x01"); return; }
    mark_pass();
}