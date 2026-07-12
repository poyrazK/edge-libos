// write(1, "hello\n", 6): must return 6 bytes written.
#include "syscall.h"

__attribute__((visibility("default")))
void _start(void) {
    static const char msg[] = "hello\n";
    int64_t n = sc3(NR_WRITE, 1 /*stdout*/, (int64_t)(intptr_t)msg, 6);
    if (n == 6) mark_pass();
    else mark_fail("write != 6");
}