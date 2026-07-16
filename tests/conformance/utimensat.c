// utimensat(path, times, flags) — set timestamps; NULL times = now.
//
// We can't easily verify the timestamp from inside the kernel, so just
// confirm the syscall succeeds (rc == 0).
#include "syscall.h"

__attribute__((visibility("default")))
void _start(void) {
    char *buf = (char *)(intptr_t)MARKER_ADDR;
    const char *s = "utime_file";
    for (int i = 0; s[i]; i++) buf[i] = s[i]; buf[10] = 0;

    // Create the file via openat(O_WRONLY|O_CREAT|O_EXCL).
    int64_t open_ret = sc4(NR_OPENAT, -100, (int64_t)(intptr_t)buf, 193, 420);
    if (open_ret == -17 /*-EEXIST*/) {
        mark_skip("utime_file leftover from prior run");
        return;
    }
    if (open_ret < 0) { mark_fail("openat"); return; }
    (void)sc1(NR_CLOSE, (int)open_ret);

    // utimensat with times=NULL → set both to now.
    int64_t r = sc4(NR_UTIMENSAT, -100, (int64_t)(intptr_t)buf, 0, 0);
    if (r != 0) { mark_fail("utimensat NULL times failed"); return; }

    // Explicit times: atime=1000s, mtime=2000s, nsec=0 each.
    // wasm32-musl struct timespec = {i64 tv_sec, i64 tv_nsec} = 16 bytes.
    char *ts = buf + 64;
    // 8 bytes for sec, 8 bytes for nsec. Use int64_t store via __kernel_syscall return.
    // Simpler: write 8 little-endian bytes via plain arithmetic.
    for (int i = 0; i < 16; i++) ts[i] = 0;
    // tv_sec = 1000 little-endian.
    ts[0] = (char)0xe8; ts[1] = (char)0x03; ts[2] = 0; ts[3] = 0;
    ts[4] = 0; ts[5] = 0; ts[6] = 0; ts[7] = 0;
    // tv_nsec = 0 (already).
    // 2nd timespec starts at ts+16: tv_sec = 2000.
    ts[16] = (char)0xd0; ts[17] = (char)0x07; ts[18] = 0; ts[19] = 0;
    ts[20] = 0; ts[21] = 0; ts[22] = 0; ts[23] = 0;
    // 2nd nsec = 0.

    int64_t r2 = sc4(NR_UTIMENSAT, -100, (int64_t)(intptr_t)buf, (int64_t)(intptr_t)ts, 0);
    if (r2 != 0) { mark_fail("utimensat explicit failed"); return; }

    (void)sc1(NR_UNLINK, (int64_t)(intptr_t)buf);
    mark_pass();
}