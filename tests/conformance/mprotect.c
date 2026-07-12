// mprotect is a no-op in P0; always returns 0.
#include "syscall.h"

__attribute__((visibility("default")))
void _start(void) {
    int64_t r = sc3(NR_MPROTECT, 0, 4096, 0 /*PROT_NONE*/);
    if (r == 0) mark_pass();
    else mark_fail("mprotect returned non-zero");
}