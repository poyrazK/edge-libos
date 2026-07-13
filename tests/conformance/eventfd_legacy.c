// eventfd(2) — legacy entry, no flags. Implemented as a shim over
// eventfd2 — so this test is essentially a parallel of eventfd2.

#include "syscall.h"

__attribute__((visibility("default")))
void _start(void) {
    // initval=0, flags=EFD_NONBLOCK — read returns -EAGAIN when empty
    // (matches the trace-host harness model: no infinite blocking).
    int64_t fd = sc2(NR_EVENTFD, 0 /*initval*/, 0x4000 /*EFD_NONBLOCK*/);
    if (fd < 0) {
        mark_fail("eventfd legacy failed");
        return;
    }

    // Write a u64 = 1 to the counter (8 bytes LE).
    char *buf = (char *)(intptr_t)(MARKER_ADDR + 100);
    buf[0] = 1; buf[1] = 0; buf[2] = 0; buf[3] = 0;
    buf[4] = 0; buf[5] = 0; buf[6] = 0; buf[7] = 0;
    int64_t wr = sc3(NR_WRITE, (uint32_t)fd,
                     (int64_t)(intptr_t)buf, 8);
    if (wr != 8) {
        mark_fail("write to eventfd");
        return;
    }

    // Read it back.
    char *out = (char *)(intptr_t)(MARKER_ADDR + 200);
    int64_t rd = sc3(NR_READ, (uint32_t)fd, (int64_t)(intptr_t)out, 8);
    if (rd != 8) {
        mark_fail("read from eventfd returned wrong size");
        return;
    }
    // out[0] should be 1 (little-endian u64).
    if ((uint8_t)out[0] != 1) {
        mark_fail("counter should be 1");
        return;
    }

    // Second read on empty counter: with EFD_NONBLOCK the kernel returns
    // -EAGAIN. Our v1 model also accepts 0 (blocking read with empty
    // counter collapses to EOF in the trace-host harness).
    int64_t rd2 = sc3(NR_READ, (uint32_t)fd,
                      (int64_t)(intptr_t)(MARKER_ADDR + 300), 8);
    if (rd2 != -11 /*EAGAIN*/ && rd2 != 0) {
        mark_fail("second read on empty eventfd should be EAGAIN or 0");
        return;
    }

    sc1(NR_CLOSE, (uint32_t)fd);
    mark_pass();
}