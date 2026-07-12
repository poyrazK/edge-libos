// munmap(addr, len). First mmap a region, then munmap it.
#include "syscall.h"

#define PROT_READ  0x1
#define PROT_WRITE 0x2
#define MAP_ANONYMOUS 0x20
#define MAP_PRIVATE   0x02

__attribute__((visibility("default")))
void _start(void) {
    int64_t addr = sc6(NR_MMAP, 0, 4096,
                       PROT_READ | PROT_WRITE,
                       MAP_ANONYMOUS | MAP_PRIVATE,
                       -1, 0);
    if (addr <= 0) { mark_fail("mmap failed"); return; }
    int64_t r = sc2(NR_MUNMAP, addr, 4096);
    if (r == 0) mark_pass();
    else mark_fail("munmap returned non-zero");
}