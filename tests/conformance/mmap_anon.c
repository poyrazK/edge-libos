// mmap(NULL, 4096, PROT_READ|PROT_WRITE, MAP_ANONYMOUS|MAP_PRIVATE, -1, 0).
// P0 supports anonymous private only. Other flag combos return -ENOSYS.
#include "syscall.h"

#define PROT_READ  0x1
#define PROT_WRITE 0x2
#define MAP_ANONYMOUS 0x20
#define MAP_PRIVATE   0x02

__attribute__((visibility("default")))
void _start(void) {
    int64_t addr = sc6(NR_MMAP,
                       0,        // addr hint
                       4096,     // len
                       PROT_READ | PROT_WRITE,
                       MAP_ANONYMOUS | MAP_PRIVATE,
                       -1,       // fd
                       0);       // off
    if (addr > 0) mark_pass();
    else mark_fail("mmap_anon returned non-positive");
}