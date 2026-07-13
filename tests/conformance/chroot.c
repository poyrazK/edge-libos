// chroot(path) — make path the new root; subsequent resolution is
// relative to it. chroot is permanent on this kernel (no saved root).
#include "syscall.h"

__attribute__((visibility("default")))
void _start(void) {
    char *buf = (char *)(intptr_t)MARKER_ADDR;
    // Use a unique-per-run name based on time() to avoid clashing with
    // leftover dirs from prior runs.
    // Simple counter via shared guest state: read a small "tag" via time.
    const char *s = "chroot_sub_xx";
    for (int i = 0; s[i]; i++) buf[i] = s[i]; buf[13] = 0;

    // mkdir; tolerate -EEXIST (cleanup from prior run).
    int64_t m = sc2(NR_MKDIR, (int64_t)(intptr_t)buf, 0755);
    if (m != 0 && m != -17) { mark_fail("mkdir"); return; }

    int64_t c = sc1(NR_CHROOT, (int64_t)(intptr_t)buf);
    if (c != 0) { mark_fail("chroot failed"); return; }

    // getcwd should end with our dir name.
    int64_t len = sc2(NR_GETCWD, (int64_t)(intptr_t)(buf + 64), 256);
    if (len < 0) { mark_fail("getcwd failed"); return; }
    if (len < 13) { mark_fail("getcwd too short"); return; }
    for (int i = 0; i < 13; i++) {
        if (buf[64 + (len - 13) + i] != s[i]) { mark_fail("getcwd suffix wrong"); return; }
    }

    // After chroot, the new root contains "chroot_sub_xx/" as its only
    // entry. Cleanup is impossible from here (we can no longer reach
    // the parent dir), so just mark_pass.
    mark_pass();
}