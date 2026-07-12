// lseek on a pipe or opened directory returns -ESPIPE (-29). P0 has no
// seekable resources yet; the only consistent assertion is -ESPIPE.
#include "syscall.h"

#define SEEK_SET 0

__attribute__((visibility("default")))
void _start(void) {
    int64_t r = sc3(NR_LSEEK, 1 /*stdout*/, 5, SEEK_SET);
    if (r == -29) mark_pass(); // -ESPIPE
    else mark_fail("lseek on stream did not return -ESPIPE");
}