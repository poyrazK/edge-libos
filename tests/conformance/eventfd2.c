// eventfd2(2) — allocate an eventfd, write a u64 counter into it,
// read it back, expect the same value.

#include "syscall.h"

void _start(void) {
    int64_t fd = sc2(NR_EVENTFD2, 0, 0);
    if (fd < 0) {
        mark_fail("eventfd2");
        return;
    }

    // Write a u64 = 42 at marker address (just below the marker string).
    uint64_t val = 42;
    // NR_WRITE = 1. Args: (fd, buf_ptr, len, ...).
    int64_t wn = sc3(NR_WRITE, fd, (int64_t)(intptr_t)&val, 8);
    if (wn != 8) {
        mark_fail("write eventfd");
        return;
    }

    // Read it back. NR_READ = 0.
    uint64_t out = 0;
    int64_t rn = sc3(NR_READ, fd, (int64_t)(intptr_t)&out, 8);
    if (rn != 8) {
        mark_fail("read eventfd");
        return;
    }
    if (out != 42) {
        mark_fail("counter mismatch");
        return;
    }

    mark_pass();
}