// socket(AF_INET, SOCK_STREAM, 0): returns a new fd (>= 3 after stdio).
#include "syscall.h"

__attribute__((visibility("default")))
void _start(void) {
    int64_t fd = sc3(NR_SOCKET, 2 /*AF_INET*/, 1 /*SOCK_STREAM*/, 0);
    if (fd >= 3) mark_pass();
    else mark_fail("socket returned invalid fd");
}