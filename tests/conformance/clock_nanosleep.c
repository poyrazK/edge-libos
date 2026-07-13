// clock_nanosleep(CLOCK_REALTIME, 0, &ts, NULL) — sleep 1ms; return 0.
#include "syscall.h"

__attribute__((visibility("default")))
void _start(void) {
    char *buf = (char *)(intptr_t)MARKER_ADDR;
    // ts = {0, 1_000_000} (= 1ms) little-endian.
    for (int i = 0; i < 16; i++) buf[i] = 0;
    // tv_nsec at offset 8.
    int64_t nsec = 1000000;
    for (int i = 0; i < 8; i++) {
        buf[8 + i] = (char)((nsec >> (8 * i)) & 0xff);
    }

    int64_t r = sc4(NR_CLOCK_NANOSLEEP, 0, 0, (int64_t)(intptr_t)buf, 0);
    if (r != 0) { mark_fail("clock_nanosleep failed"); return; }
    mark_pass();
}