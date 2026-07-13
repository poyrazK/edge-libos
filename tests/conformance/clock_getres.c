// clock_getres(CLOCK_REALTIME, &ts) → 0; ts = {0, 1} (1ns resolution).
#include "syscall.h"

__attribute__((visibility("default")))
void _start(void) {
    char *buf = (char *)(intptr_t)MARKER_ADDR;
    int64_t r = sc2(NR_CLOCK_GETRES, 0, (int64_t)(intptr_t)buf); // CLOCK_REALTIME = 0
    if (r != 0) { mark_fail("clock_getres failed"); return; }
    int64_t nsec = 0;
    for (int i = 0; i < 8; i++) {
        nsec |= ((int64_t)(unsigned char)buf[8 + i]) << (8 * i);
    }
    if (nsec != 1) { mark_fail("resolution nsec != 1"); return; }
    mark_pass();
}