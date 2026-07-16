// open(path, O_RDONLY, 0): returns a new fd (>= 3 after stdio). The
// conformance harness preopens "/" so a fresh test directory exists.
#include "syscall.h"

__attribute__((visibility("default")))
void _start(void) {
    // path "/\0" at MARKER_ADDR + 1024 keeps the marker region intact.
    char *p = (char *)(intptr_t)(MARKER_ADDR + 1024);
    p[0] = '/';
    p[1] = 0;
    int64_t fd = sc3(NR_OPEN, (int64_t)(intptr_t)p, 0 /*O_RDONLY*/, 0);
    if (fd == -2 /*ENOENT*/) {
        mark_skip("preopen root lacks /");
        return;
    }
    if (fd >= 3) mark_pass();
    else mark_fail("open returned invalid fd");
}
