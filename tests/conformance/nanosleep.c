// nanosleep(0): zero-duration sleep returns 0.
#include "syscall.h"

typedef struct { int64_t sec; int64_t nsec; } timespec_t;

__attribute__((visibility("default")))
void _start(void) {
    timespec_t req = {0, 0};
    timespec_t rem = {42, 42};
    int64_t r = sc2(NR_NANOSLEEP, (int64_t)(intptr_t)&req, (int64_t)(intptr_t)&rem);
    if (r == 0) mark_pass();
    else mark_fail("nanosleep != 0");
}