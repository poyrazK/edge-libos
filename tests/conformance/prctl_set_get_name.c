// prctl(PR_SET_NAME, "edge") then PR_GET_NAME → "edge".
#include "syscall.h"

#define PR_SET_NAME 15
#define PR_GET_NAME 16

__attribute__((visibility("default")))
void _start(void) {
    char *buf = (char *)(intptr_t)MARKER_ADDR;
    // "edge\0...." at offset 0.
    buf[0] = 'e'; buf[1] = 'd'; buf[2] = 'g'; buf[3] = 'e'; buf[4] = 0;
    for (int i = 5; i < 16; i++) buf[i] = 0;
    // out buffer at offset 16.
    char *out = buf + 16;

    int64_t r1 = sc5(NR_PRCTL, PR_SET_NAME, (int64_t)(intptr_t)buf, 0, 0, 0);
    if (r1 != 0) { mark_fail("PR_SET_NAME failed"); return; }

    int64_t r2 = sc5(NR_PRCTL, PR_GET_NAME, (int64_t)(intptr_t)out, 0, 0, 0);
    if (r2 != 0) { mark_fail("PR_GET_NAME failed"); return; }

    if (!(out[0] == 'e' && out[1] == 'd' && out[2] == 'g' && out[3] == 'e' && out[4] == 0)) {
        mark_fail("name roundtrip wrong");
        return;
    }
    mark_pass();
}