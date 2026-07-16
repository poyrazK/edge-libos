// getdents64 stream position: open a directory, call getdents64 with a
// small buffer so it takes multiple calls to drain the listing; the
// last call must return 0.
//
// This validates P2-B2 (getdents64 stream position). The build pipeline
// creates the dir `getdents64_dir/` with three files before compile:
//   - foo
//   - bar
//   - baz
//
// We pass the dir path as argv[1] via NR_OPENAT. Since zig cc doesn't
// pre-link argv, we hardcode the dir name here — the build pipeline
// (runner.sh) creates it under the preopen root and the test resolves
// it via AT_FDCWD + relative path.

#include "syscall.h"

static int my_strcmp(const char *a, const char *b) {
    while (*a && *a == *b) { a++; b++; }
    return *(unsigned char *)a - *(unsigned char *)b;
}

static int my_strlen(const char *s) { int n = 0; while (s[n]) n++; return n; }

static void put(const char *s) {
    sc3(NR_WRITE, 1, (int64_t)(intptr_t)s, my_strlen(s));
}

__attribute__((visibility("default")))
void _start(void) {
    // Open the directory. AT_FDCWD = -100.
    int64_t dirfd = sc4(NR_OPENAT, -100,
                        (int64_t)(intptr_t)"getdents64_dir",
                        0 /*O_RDONLY*/, 0);
    if (dirfd == -2 /*ENOENT*/) {
        // The runner should have pre-created getdents64_dir under the
        // preopen root; if it didn't (or the preopen doesn't expose
        // the cwd), degrade to SKIP rather than fail.
        mark_skip("getdents64_dir missing from preopen");
        return;
    }
    if (dirfd < 0) {
        mark_fail("openat dir");
        return;
    }

    // Read with a small buffer: 64 bytes. Three names sorted alpha =
    // bar, baz, foo. Each dirent64 record is 24 bytes + name length:
    //   bar  = 27
    //   baz  = 27
    //   foo  = 27
    // Total = 81. So a 64-byte buf takes 2 calls (first 64, second 17)
    // then a 3rd call returns 0.
    char buf[64];
    int64_t r1 = sc3(NR_GETDENTS64, dirfd, (int64_t)(intptr_t)buf, 64);
    if (r1 <= 0) {
        mark_fail("first getdents64");
        return;
    }
    int64_t r2 = sc3(NR_GETDENTS64, dirfd, (int64_t)(intptr_t)buf, 64);
    if (r2 <= 0) {
        mark_fail("second getdents64");
        return;
    }
    // First + second should equal the total encoding length; check
    // sum covers all three names.
    if (r1 + r2 != 27 * 3) {
        put("sum mismatch\n");
        mark_fail("sum != 81");
        return;
    }
    int64_t r3 = sc3(NR_GETDENTS64, dirfd, (int64_t)(intptr_t)buf, 64);
    if (r3 != 0) {
        mark_fail("third getdents64 should be 0");
        return;
    }

    // Rewind via lseek(SEEK_SET, 0) and confirm we get r1 again.
    int64_t lr = sc3(NR_LSEEK, dirfd, 0, 0 /*SEEK_SET*/);
    if (lr != 0) {
        mark_fail("lseek rewind");
        return;
    }
    int64_t r4 = sc3(NR_GETDENTS64, dirfd, (int64_t)(intptr_t)buf, 64);
    if (r4 != r1) {
        mark_fail("after rewind, first read differs");
        return;
    }

    // Close the dir.
    sc1(NR_CLOSE, dirfd);

    mark_pass();
}