// uname(buf) — fill a 390-byte struct with 6×65-byte strings.
#include "syscall.h"

__attribute__((visibility("default")))
void _start(void) {
    char *buf = (char *)(intptr_t)MARKER_ADDR;
    int64_t r = sc1(NR_UNAME, (int64_t)(intptr_t)buf);
    if (r != 0) { mark_fail("uname failed"); return; }
    // sysname (offset 0) should be "Linux".
    if (!(buf[0] == 'L' && buf[1] == 'i' && buf[2] == 'n' && buf[3] == 'u' && buf[4] == 'x'
          && buf[5] == 0)) {
        mark_fail("sysname != Linux");
        return;
    }
    // machine (offset 65*4 = 260) should start with "wasm32".
    char *m = buf + 65 * 4;
    if (!(m[0] == 'w' && m[1] == 'a' && m[2] == 's' && m[3] == 'm'
          && m[4] == '3' && m[5] == '2' && m[6] == 0)) {
        mark_fail("machine != wasm32");
        return;
    }
    mark_pass();
}