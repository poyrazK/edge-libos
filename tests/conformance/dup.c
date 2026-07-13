// dup(rd) → new fd that shares the read end's open-file description.
//
// We pipe2() two ends, write one byte, and confirm both the original
// read fd and the dup'd fd return that byte. Both reads see the same
// shared offset — proves the fds share state via Arc<Mutex<>>.
//
// mark_pass when:
//   - dup returns a fresh fd != rd
//   - read from rd returns the byte
//   - the dup'd fd also returns that byte (shared offset)
#include "syscall.h"

__attribute__((visibility("default")))
void _start(void) {
    // pipe2 fds land in MARKER_ADDR (16 bytes = 2 u32s).
    int32_t *fds_arr = (int32_t *)(intptr_t)MARKER_ADDR;
    int64_t pipe2_ret = sc2(NR_PIPE2, (int64_t)(intptr_t)fds_arr, 0);
    if (pipe2_ret != 0) { mark_fail("pipe2 failed"); return; }

    int rd = (int)fds_arr[0];
    int wr = (int)fds_arr[1];

    // Write one byte.
    char *one = (char *)(intptr_t)(MARKER_ADDR + 4096);
    one[0] = 'X';
    int64_t write_ret = sc3(NR_WRITE, wr, (int64_t)(intptr_t)one, 1);
    if (write_ret != 1) { mark_fail("write failed"); return; }

    // dup(rd).
    int64_t dup_ret = sc1(NR_DUP, rd);
    if (dup_ret < 0) { mark_fail("dup returned negative"); return; }
    int dupped = (int)dup_ret;
    if (dupped == rd) { mark_fail("dup returned same fd"); return; }

    // Read from the original fd (consumes the shared offset).
    char *buf_a = (char *)(intptr_t)(MARKER_ADDR + 8192);
    int64_t read_a = sc3(NR_READ, rd, (int64_t)(intptr_t)buf_a, 1);
    if (read_a != 1 || buf_a[0] != 'X') { mark_fail("read from rd"); return; }

    // Write another byte; the dup'd fd should see it next.
    one[0] = 'Y';
    int64_t write_ret2 = sc3(NR_WRITE, wr, (int64_t)(intptr_t)one, 1);
    if (write_ret2 != 1) { mark_fail("second write failed"); return; }

    // Read from the dup'd fd: should also return 'Y' (shared buffer).
    char *buf_b = (char *)(intptr_t)(MARKER_ADDR + 8192 + 16);
    int64_t read_b = sc3(NR_READ, dupped, (int64_t)(intptr_t)buf_b, 1);
    if (read_b != 1 || buf_b[0] != 'Y') { mark_fail("dup'd fd couldn't read shared byte"); return; }

    mark_pass();
}
