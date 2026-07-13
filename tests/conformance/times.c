// times(2) stub — returns 0 clock ticks per the P2 stub contract.
// We verify the call returns 0 and writes zeros to the tms buffer.

#include "syscall.h"

__attribute__((visibility("default")))
void _start(void) {
    char buf[32];
    for (int i = 0; i < 32; i++) buf[i] = 0xff;  // poison so we can detect the write.

    int64_t rc = sc1(NR_TIMES, (int64_t)(intptr_t)buf);
    if (rc != 0) {
        mark_fail("times should return 0 clock ticks");
        return;
    }

    // All 32 bytes should be zero after the kernel wrote them.
    for (int i = 0; i < 32; i++) {
        if (buf[i] != 0) {
            mark_fail("times did not zero-fill tms buffer");
            return;
        }
    }

    // NULL buf → -EFAULT.
    int64_t rc_bad = sc1(NR_TIMES, 0);
    if (rc_bad != -14 /*EFAULT*/) {
        mark_fail("times(NULL) should return EFAULT");
        return;
    }

    mark_pass();
}