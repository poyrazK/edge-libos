// stat("/", &statbuf): returns 0 on success.
#include "syscall.h"

__attribute__((visibility("default")))
void _start(void) {
    char *p = (char *)(intptr_t)(MARKER_ADDR + 1024);
    p[0] = '/';
    p[1] = 0;
    char *statbuf = (char *)(intptr_t)(MARKER_ADDR + 2048);
    int64_t r = sc2(NR_STAT, (int64_t)(intptr_t)p, (int64_t)(intptr_t)statbuf);
    if (r == 0) mark_pass();
    else mark_fail("stat returned non-zero");
}