// writev(stdout, iov, 2): concatenates "foo" + "bar" → "foobar" into
// the host's stdout buffer, which the runner inspects via Kernel's
// stdout_buf() / trace-host output.
#include "syscall.h"
#include <stdint.h>

__attribute__((visibility("default")))
void _start(void) {
    char *b1 = (char *)(intptr_t)(MARKER_ADDR + 1100);
    b1[0] = 'f'; b1[1] = 'o'; b1[2] = 'b';
    char *b2 = (char *)(intptr_t)(MARKER_ADDR + 1200);
    b2[0] = 'b'; b2[1] = 'a'; b2[2] = 'r';

    uint32_t *iov = (uint32_t *)(intptr_t)(MARKER_ADDR + 1300);
    iov[0] = MARKER_ADDR + 1100;  iov[1] = 3;
    iov[2] = MARKER_ADDR + 1200;  iov[3] = 3;

    int64_t n = sc3(NR_WRITEV, 1 /*STDOUT*/, (int64_t)(intptr_t)iov, 2);
    if (n == 6) mark_pass();
    else mark_fail("writev != 6");
}