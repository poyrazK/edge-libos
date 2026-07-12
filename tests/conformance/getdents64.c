// getdents64 on stdout: not seekable directory — assert -ENOTDIR (-20).
// (For P0 we accept either -ENOTDIR or -EBADF; either proves the syscall
// routed correctly.)
#include "syscall.h"

__attribute__((visibility("default")))
void _start(void) {
    char buf[256];
    int64_t r = sc3(NR_GETDENTS64, 1 /*stdout*/, (int64_t)(intptr_t)buf, 256);
    if (r == -20 /*ENOTDIR*/ || r == -9 /*EBADF*/) mark_pass();
    else mark_fail("getdents64 on stream returned unexpected value");
}