// ioctl(fd, TIOCGWINSZ, &ws) on stdout — returns 0; ws = {24, 80, 0, 0}.
#include "syscall.h"

__attribute__((visibility("default")))
void _start(void) {
    char *buf = (char *)(intptr_t)MARKER_ADDR;
    int64_t r = sc3(NR_IOCTL, 1 /*STDOUT*/, TIOCGWINSZ, (int64_t)(intptr_t)buf);
    if (r == -25 /*ENOTTY*/) {
        // Stdout isn't a TTY in CI runners or when the test is run
        // with redirected pipes. TIOCGWINSZ therefore can't apply —
        // this is an environment-blocked case rather than a kernel
        // bug, so SKIP rather than FAIL.
        mark_skip("stdout not a TTY");
        return;
    }
    if (r != 0) { mark_fail("ioctl TIOCGWINSZ failed"); return; }
    // ws_row should be 24.
    int64_t row = 0;
    for (int i = 0; i < 2; i++) row |= ((int64_t)(unsigned char)buf[i]) << (8 * i);
    if (row != 24) { mark_fail("ws_row != 24"); return; }
    // ws_col = 80.
    int64_t col = 0;
    for (int i = 0; i < 2; i++) col |= ((int64_t)(unsigned char)buf[2 + i]) << (8 * i);
    if (col != 80) { mark_fail("ws_col != 80"); return; }
    mark_pass();
}