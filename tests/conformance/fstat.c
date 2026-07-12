// fstat(1, &stat): writes 120-byte stat buffer. Read back and verify
// st_mode is non-zero (synthesized as S_IFCHR | 0666 for streams).
#include "syscall.h"

typedef struct {
    uint64_t st_dev;        //  0
    uint64_t st_ino;        //  8
    uint64_t st_nlink;      // 16
    uint32_t st_mode;       // 24
    uint32_t st_uid;        // 28
    uint32_t st_gid;        // 32
    uint32_t _pad0;         // 36
    uint64_t st_rdev;       // 40
    int64_t  st_size;       // 48
    int64_t  st_blksize;    // 56
    int64_t  st_blocks;     // 64
    int64_t  st_atime;      // 72
    int64_t  st_atime_nsec; // 80
    int64_t  st_mtime;      // 88
    int64_t  st_mtime_nsec; // 96
    int64_t  st_ctime;      // 104
    int64_t  st_ctime_nsec; // 112
} __attribute__((packed)) stat_t;

__attribute__((visibility("default")))
void _start(void) {
    stat_t s;
    int64_t r = sc2(NR_FSTAT, 1 /*stdout*/, (int64_t)(intptr_t)&s);
    if (r != 0) { mark_fail("fstat returned errno"); return; }
    if (s.st_mode != 0) mark_pass();
    else mark_fail("st_mode is zero");
}