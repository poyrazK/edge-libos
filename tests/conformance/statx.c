// statx(AT_FDCWD, "/", 0, mask, &buf) — extended stat.
//
// Linux x86-64 struct statx is 256 bytes (linux/stat.h). We mirror only
// the first ~80 bytes in C since musl doesn't fully decode the trailing
// reserved area. The kernel writes little-endian at exact offsets;
// stx_mode at offset 32 (u16) and stx_mask at offset 0 (u32) are
// enough to verify the contract.
//
// mark_pass when:
//   - syscall returns 0 (success)
//   - stx_mask != 0 (we report the filled fields)
//   - stx_mode != 0 (file type bits must be set; this is a directory)
#include "syscall.h"

// Mirrors the first 80 bytes of struct statx. The kernel writes
// each field at its exact offset — see src/sys/file.rs::Statx::encode.
typedef struct {
    uint32_t stx_mask;          //  0
    uint32_t stx_blksize;       //  4
    uint64_t stx_attributes;    //  8
    uint64_t stx_nlink;         // 16
    uint32_t stx_uid;           // 24
    uint32_t stx_gid;           // 28
    uint16_t stx_mode;          // 32
    uint16_t _pad0;             // 34
    uint32_t _pad1;             // 36
    uint64_t stx_ino;           // 40
    uint64_t stx_size;          // 48
    uint64_t stx_blocks;        // 56
    uint64_t stx_attributes_mask; // 64
    uint64_t stx_atime_sec;     // 72
} __attribute__((packed)) statx_prefix;

__attribute__((visibility("default")))
void _start(void) {
    // Path "/\0" at MARKER_ADDR + 1024.
    char *path = (char *)(intptr_t)(MARKER_ADDR + 1024);
    path[0] = '/';
    path[1] = 0;

    // statx buffer (256 B) at MARKER_ADDR + 4096.
    char *buf = (char *)(intptr_t)(MARKER_ADDR + 4096);

    // Mask: TYPE | MODE | NLINK | UID | GID | ATIME | MTIME | CTIME | INO | SIZE | BLOCKS
    // = 0x1 | 0x2 | 0x4 | 0x8 | 0x10 | 0x20 | 0x40 | 0x80 | 0x100 | 0x200 | 0x400 = 0x7ff
    int64_t mask = 0x7ff;

    int64_t r = sc5(NR_STATX,
                    (int64_t)AT_FDCWD,                // dirfd
                    (int64_t)(intptr_t)path,          // pathname
                    0,                                // flags
                    mask,                             // mask
                    (int64_t)(intptr_t)buf);          // buf

    if (r == -2 /*ENOENT*/) {
        // Preopen root doesn't expose "/" on this host — degrade
        // the rest of the test to SKIP rather than fail on an
        // env-blocked path resolution.
        mark_skip("preopen root lacks /");
        return;
    }
    if (r != 0) { mark_fail("statx returned non-zero"); return; }

    statx_prefix *sx = (statx_prefix *)(intptr_t)buf;
    if (sx->stx_mask == 0) {
        mark_fail("stx_mask is zero");
        return;
    }
    if (sx->stx_mode == 0) {
        mark_fail("stx_mode is zero");
        return;
    }
    // stx_size for a directory may be 0 on some hosts; we don't assert
    // a positive size, just that the syscall worked end-to-end.
    mark_pass();
}