// clock_gettime(CLOCK_MONOTONIC, timespec*): writes 16 bytes (sec + nsec).
// nsec must be < 1e9.
#include "syscall.h"

typedef struct { int64_t sec; int64_t nsec; } timespec_t;

__attribute__((visibility("default")))
void _start(void) {
    timespec_t ts;
    int64_t r = sc2(NR_CLOCK_GETTIME, 1 /*MONOTONIC*/, (int64_t)(intptr_t)&ts);
    if (r != 0) { mark_fail("clock_gettime returned errno"); return; }
    if (ts.nsec >= 0 && ts.nsec < 1000000000LL) mark_pass();
    else mark_fail("nsec out of range");
}