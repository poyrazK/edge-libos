// exit(0) succeeds — the host driver propagates the code.
//
// P1 ships the `exit` syscall implemented to record the code on
// Kernel::exit_code and return 0 to the caller (rather than trap),
// so trace-host can keep running after the guest's _start returns.
// We mark_pass() before exit() (the kernel doesn't touch the marker
// region). After exit() returns we immediately trap the wasm via
// unreachable so trace-host sees a clean stop and the marker is in
// place when read.
#include "syscall.h"

__attribute__((visibility("default")))
void _start(void) {
    mark_pass();
    sc1(NR_EXIT, 0);
    // If we got here, the kernel returned instead of trapping — that's
    // expected (P1 behavior). Trap the wasm to signal end-of-run.
    __builtin_trap();
}