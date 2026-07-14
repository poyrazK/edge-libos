// P3 Tier-4 clone(56) v1 conformance:
//   - clone(0) with no supported flags → -EINVAL (-22)
//   - clone(CLONE_CHILD_SETTID | CLONE_PARENT_SETTID, …) → new PID > 1
//     and BOTH TID writebacks carry the new PID.
//
// The v1 handler supports only the two TID-writeback flag bits
// (0x01000000 and 0x08000000). Any other bit → -EINVAL.
//
// Layout: ptid and ctid are 8-byte aligned slots in BSS at fixed
// addresses the fixture reserves (`.bss.ptid` / `.bss.ctid`).

#include "syscall.h"

#define CLONE_CHILD_SETTID  0x01000000
#define CLONE_PARENT_SETTID 0x08000000

static int ptid __attribute__((aligned(8)));
static int ctid __attribute__((aligned(8)));

__attribute__((visibility("default")))
void _start(void) {
    // Case 1: no supported flags → -EINVAL.
    int64_t r = sc6(NR_CLONE, 0, 0, 0, 0, 0, 0);
    if (r != -22) { mark_fail("clone(0) != -EINVAL"); return; }

    // Case 2: BOTH TID-writeback flags set, valid pointers.
    // Write sentinel values so we can detect "did the kernel write".
    ptid = 0xdeadbeef;
    ctid = 0xcafebabe;
    int flags = CLONE_CHILD_SETTID | CLONE_PARENT_SETTID;
    r = sc6(NR_CLONE, flags, 0, (int64_t)&ptid, (int64_t)&ctid, 0, 0);

    if (r <= 1) { mark_fail("clone() did not return child_pid > 1"); return; }
    if (r > 0x7fffffff) { mark_fail("clone() returned bogus child_pid"); return; }

    int child = (int)r;
    if (ptid != child) { mark_fail("ptid not written with child_pid"); return; }
    if (ctid != child) { mark_fail("ctid not written with child_pid"); return; }

    mark_pass();
}