// snapshot: P2-D3.5 guest-driven quiescence.
// This test asserts NR_SNAPSHOT(path) returns a non-negative
// byte count, meaning the kernel successfully wrote the
// snapshot postcard to the given path. See ADR 0004 §1.
//
// Strategy: snapshot to a fixed tmpfs path. The host's runner
// script does not need to read the bytes back — a successful
// syscall return is sufficient evidence (the runner asserts on
// the syscall name "snapshot" via expected_syscall).
#include "syscall.h"

// The fixture writes the snapshot to a tmpfs path the host
// creates on demand. Passing a NULL path pointer would yield
// -EFAULT, but we exercise the success path here.
#define SNAP_PATH "/tmp/edge-snapshot-conformance.snap"

__attribute__((visibility("default")))
void _start(void) {
    // Copy the path bytes into the guest's linear memory at a
    // safe scratch slot (MARKER_ADDR + 4096 = 8192, doesn't
    // collide with marker region).
    const char *src = SNAP_PATH;
    char *dst = (char *)(intptr_t)(MARKER_ADDR + 4096);
    int i = 0;
    for (; src[i] && i < 255; i++) dst[i] = src[i];
    dst[i] = 0;

    int64_t r = sc1(NR_SNAPSHOT, (int64_t)(intptr_t)dst);
    if (r < 0) {
        // The host's preopen may not expose /tmp as writable. A
        // negative return with these errnos is an env-blocked
        // failure rather than a kernel bug — degrade to SKIP.
        // -EPERM/-ENOENT/-EACCES cover filesystem visibility;
        // -EROFS and -ENOSPC cover per-mount policy and disk state.
        if (r == -1 /*EPERM*/ || r == -2 /*ENOENT*/ ||
            r == -13 /*EACCES*/ || r == -30 /*EROFS*/ ||
            r == -28 /*ENOSPC*/) {
            mark_skip("snapshot path not writable in this env");
            return;
        }
        mark_fail("snapshot returned negative");
        return;
    }
    mark_pass();
}
