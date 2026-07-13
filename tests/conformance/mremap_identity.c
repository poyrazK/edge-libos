// mremap_identity — mmap(MAP_ANON, 4096), then mremap to 8192, returns same addr.
#include "syscall.h"

__attribute__((visibility("default")))
void _start(void) {
    // mmap: NR_MMAP=9. args: (addr, len, prot, flags, fd, off)
    // flags = MAP_ANON|MAP_PRIVATE = 0x22.
    int64_t addr = sc6(NR_MMAP, 0, 4096, 3 /*PROT_READ|PROT_WRITE*/, 0x22, -1, 0);
    if (addr <= 0) { mark_fail("mmap failed"); return; }

    // mremap: NR_MREMAP=25. (old, old_len, new_len, flags, new_addr)
    int64_t r = sc5(NR_MREMAP, addr, 4096, 8192, 0, 0);
    if (r != addr) { mark_fail("mremap identity failed"); return; }
    mark_pass();
}