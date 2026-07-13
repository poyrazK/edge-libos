// sysinfo(2) stub — returns fake uptime/memory per the P2 stub contract.
// We just verify the call returns 0 and writes plausible fields.

#include "syscall.h"

__attribute__((visibility("default")))
void _start(void) {
    char buf[128];
    for (int i = 0; i < 128; i++) buf[i] = 0;

    int64_t rc = sc1(NR_SYSINFO, (int64_t)(intptr_t)buf);
    if (rc != 0) {
        mark_fail("sysinfo failed");
        return;
    }

    // uptime (offset 0) should be > 0. We seeded it with 1 in the stub.
    // Read 8 bytes as int64_t LE — note: long is 32 bits on wasm32, so
    // we must use int64_t to avoid shifting a 32-bit value by 56.
    int64_t uptime = 0;
    for (int i = 0; i < 8; i++) {
        uptime |= ((int64_t)(unsigned char)buf[i]) << (i * 8);
    }
    if (uptime <= 0) {
        mark_fail("sysinfo uptime not positive");
        return;
    }

    // NULL info → -EFAULT.
    int64_t rc_bad = sc1(NR_SYSINFO, 0);
    if (rc_bad != -14 /*EFAULT*/) {
        mark_fail("sysinfo(NULL) should return EFAULT");
        return;
    }

    mark_pass();
}