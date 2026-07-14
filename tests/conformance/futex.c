// futex: P3 Tier-1 implements FUTEX_WAIT + FUTEX_WAKE.
// This test asserts FUTEX_WAKE (with FUTEX_PRIVATE_FLAG) on a never-waited
// address returns 0 — the minimum-viable contract check.
// See docs/adr/0001-p3-futex-semantics.md for the full design contract.
#include "syscall.h"

#define FUTEX_CMD_WAKE 1
#define FUTEX_PRIVATE_FLAG 0x80

// Use scratch slot at MARKER_ADDR + 4096 (= 8192 today). Anchoring the
// offset to MARKER_ADDR keeps the test robust if MARKER_ADDR ever moves;
// mark_pass()/mark_fail() overwrites the first 64 bytes at MARKER_ADDR
// on entry (see tests/conformance/syscall.h).
#define FUTEX_WORD_ADDR (MARKER_ADDR + 4096)

__attribute__((visibility("default")))
void _start(void) {
    volatile uint32_t *futex_word = (volatile uint32_t *)FUTEX_WORD_ADDR;
    *futex_word = 0;
    int64_t r = sc6(NR_FUTEX,
                    (int64_t)(intptr_t)futex_word,
                    FUTEX_CMD_WAKE | FUTEX_PRIVATE_FLAG,
                    1, 0, 0, 0);
    if (r == 0) mark_pass();
    else mark_fail("futex_wake != 0");
}