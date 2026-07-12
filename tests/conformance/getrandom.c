// getrandom(buf, 32, 0): fills 32 bytes; assert at least one non-zero.
#include "syscall.h"

__attribute__((visibility("default")))
void _start(void) {
    char buf[32] = {0};
    int64_t n = sc3(NR_GETRANDOM, (int64_t)(intptr_t)buf, 32, 0);
    if (n != 32) { mark_fail("getrandom != 32"); return; }
    int any = 0;
    for (int i = 0; i < 32; i++) if (buf[i]) any = 1;
    if (any) mark_pass();
    else mark_fail("getrandom buffer all zero");
}