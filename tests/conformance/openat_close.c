// openat(AT_FDCWD, "/dev/null", O_WRONLY): assert returns fd >= 3, then close.
// For P0 conformance we open any path under the preopen. Since the preopen
// at runtime is the cwd, we open "/" and assert a valid fd.
#include "syscall.h"

#define AT_FDCWD (-100)
#define O_RDONLY 0
#define O_WRONLY 1
#define O_RDWR   2
#define O_CREAT  0x40
#define O_TRUNC  0x200

__attribute__((visibility("default")))
void _start(void) {
    static const char path[] = "/";
    int64_t fd = sc4(NR_OPENAT, AT_FDCWD,
                     (int64_t)(intptr_t)path,
                     O_RDONLY, 0);
    if (fd < 3) { mark_fail("openat returned < 3"); return; }
    int64_t r = sc1(NR_CLOSE, fd);
    if (r == 0) mark_pass();
    else mark_fail("close returned non-zero");
}