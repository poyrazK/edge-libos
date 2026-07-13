// sigaltstack(ss, old_ss) — set then query; old_ss reflects the new ss.
#include "syscall.h"

__attribute__((visibility("default")))
void _start(void) {
    char *buf = (char *)(intptr_t)MARKER_ADDR;
    // ss (offset 0) = {sp=0x1000, flags=0, size=0x2000}
    // sp at 0..8: 0x1000 little-endian.
    buf[0] = 0; buf[1] = 0x10; for (int i = 2; i < 8; i++) buf[i] = 0;
    // flags at 8..12 = 0 (already)
    // size at 16..24 = 0x2000
    for (int i = 0; i < 8; i++) buf[16 + i] = 0;
    buf[16] = 0; buf[17] = 0x20;

    // old buffer (offset 32) zeroed initially.
    int64_t r1 = sc2(NR_SIGALTSTACK, (int64_t)(intptr_t)buf, (int64_t)(intptr_t)(buf + 32));
    if (r1 != 0) { mark_fail("sigaltstack set failed"); return; }

    // Query (old only) — should still report our sp=0x1000.
    // Zero old buffer first.
    for (int i = 0; i < 24; i++) buf[32 + i] = 0;
    int64_t r2 = sc2(NR_SIGALTSTACK, 0, (int64_t)(intptr_t)(buf + 32));
    if (r2 != 0) { mark_fail("sigaltstack get failed"); return; }

    // old_sp[0..2] should be 0x10 0x00.
    if ((unsigned char)buf[32] != 0x00 || (unsigned char)buf[33] != 0x10) {
        mark_fail("old sp wrong");
        return;
    }
    mark_pass();
}