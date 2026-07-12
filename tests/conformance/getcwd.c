// getcwd(buf, 256): writes a NUL-terminated path into `buf` and returns
// the byte length excluding the NUL.
#include "syscall.h"
#include <stdint.h>

__attribute__((visibility("default")))
void _start(void) {
    char *buf = (char *)(intptr_t)(MARKER_ADDR + 1024);
    int64_t n = sc2(NR_GETCWD, (int64_t)(intptr_t)buf, 256);
    if (n <= 0) { mark_fail("getcwd returned non-positive"); return; }
    // Path must be NUL-terminated at byte n.
    if (buf[n] != 0) { mark_fail("getcwd not NUL-terminated"); return; }
    mark_pass();
}