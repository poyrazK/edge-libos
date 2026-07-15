// fork() v1 — P3 final-bundle sub-deliverable 5.
//
// v1 returns the child PID in the parent; the child fiber is
// NOT resumed (deferred-resume contract). What we can test
// from the parent:
//   1. The return value is > 0 (i.e. not -ENOSYS / not error).
//   2. The return value is > getpid() (kernel.next_pid is
//      monotonic and PID 1 is reserved for the init kernel).
//
// The child-path check (return == 0 in the child) is gated
// behind the deferred child-fiber-resume story.
#include "syscall.h"

__attribute__((visibility("default")))
void _start(void) {
    int64_t self_pid = sc1(NR_GETPID, 0);
    if (self_pid != 1) {
        mark_fail("getpid != 1 (v1 single-process model)");
        return;
    }

    int64_t child_pid = sc1(NR_FORK, 0);
    if (child_pid <= 0) {
        mark_fail("fork did not return a positive child PID");
        return;
    }
    if (child_pid <= self_pid) {
        mark_fail("fork child_pid must exceed getpid (monotonic)");
        return;
    }
    mark_pass();
}
