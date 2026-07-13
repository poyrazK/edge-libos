//! Shared header for the C conformance suite.
//!
//! Each per-syscall .c file includes this, declares its `_start` symbol,
//! and asserts via the `ASSERT_*` macros that the kernel returned the
//! expected value. `PASS:` / `FAIL:` markers are written to a known
//! memory region (`4096`) so the runner.sh can grep for them in the
//! trace-host JSON output.
//!
//! The convention: a passing test writes `PASS:<name>\0` at offset
//! 4096; a failing test writes `FAIL:<name>:<reason>\0`. The runner
//! greps the trace-host output for `PASS` in the `name` field… wait,
//! no — the runner inspects the wasm's exports after instantiation.
//!
//! For P0 we keep this minimal: each .c file simply calls the syscall
//! and emits one byte into a known buffer that the host reads back. The
//! simplest contract: write "PASS\n" to a 16-byte buffer at offset 4096
//! on success, "FAIL\n" on failure. The runner reads that buffer.

#ifndef EDGE_LIBOS_CONFORMANCE_SYSCALL_H
#define EDGE_LIBOS_CONFORMANCE_SYSCALL_H

#include <stdint.h>

// Single import. Matches `kernel.syscall` registered by `add_to_linker`.
// zig cc lowers this to the wasm import with module="kernel", name="syscall".
__attribute__((import_module("kernel"), import_name("syscall")))
int64_t __kernel_syscall(int64_t nr, int64_t a1, int64_t a2, int64_t a3,
                         int64_t a4, int64_t a5, int64_t a6);

// Linux x86-64 syscall numbers. Mirrors `src/sys/*.rs`.
#define NR_WRITE 1
#define NR_READ 0
#define NR_OPEN 2
#define NR_STAT 4
#define NR_LSTAT 6
#define NR_OPENAT 257
#define NR_READV 19
#define NR_WRITEV 20
#define NR_PIPE 22
#define NR_GETCWD 79
#define NR_CLOSE 3
#define NR_LSEEK 8
#define NR_FSTAT 5
#define NR_GETDENTS64 217
#define NR_PIPE2 293
#define NR_FCNTL 72
#define NR_DUP 32
#define NR_DUP2 33
#define NR_DUP3 292
#define O_CLOEXEC 02000000
#define F_DUPFD 0
#define F_DUPFD_CLOEXEC (1024 + 6)
#define F_GETFD 1
#define F_SETFD 2
#define F_GETFL 3
#define F_SETFL 4
#define NR_BRK 12
#define NR_MMAP 9
#define NR_MUNMAP 11
#define NR_MPROTECT 10
#define NR_CLOCK_GETTIME 228
#define NR_GETTIMEOFDAY 96
#define NR_NANOSLEEP 35
#define NR_GETRANDOM 318
#define NR_EXIT 60
#define NR_GETPID 39
#define NR_GETTID 186
#define NR_GETUID 102
#define NR_GETEUID 107
#define NR_GETGID 104
#define NR_GETEGID 108
#define NR_SET_TID_ADDRESS 218
#define NR_SET_ROBUST_LIST 273
#define NR_RT_SIGACTION 13
#define NR_RT_SIGPROCMASK 14
#define NR_ARCH_PRCTL 158
#define NR_RSEQ 334
#define NR_STATX 332
#define NR_SOCKET 41
#define NR_BIND 49
#define NR_LISTEN 50
#define NR_SETSOCKOPT 54
#define NR_GETSOCKOPT 55
#define NR_GETSOCKNAME 51
#define NR_GETPEERNAME 52
#define NR_SHUTDOWN 48
#define NR_CONNECT 42
#define NR_SENDTO 44
#define NR_RECVFROM 45
#define NR_POLL 7
#define NR_EPOLL_CREATE1 291
#define NR_EPOLL_CTL 233
#define NR_EPOLL_WAIT 232
#define NR_EVENTFD2 290

// P2-C1 part 1: mkdir / mkdirat / rmdir / unlink / unlinkat.
#define NR_MKDIR 83
#define NR_RMDIR 84
#define NR_UNLINK 87
#define NR_MKDIRAT 258
#define NR_UNLINKAT 263

// P2-C1 part 2: rename / renameat / renameat2 / truncate / ftruncate.
#define NR_RENAME 82
#define NR_RENAMEAT 264
#define NR_RENAMEAT2 316
#define NR_TRUNCATE 76
#define NR_FTRUNCATE 77

// Standard *at() dirfd values. AT_FDCWD = -100 means "use cwd".
#define AT_FDCWD (-100)

// Pass/fail markers. Placed at offset 4096 in linear memory. The runner
// reads back the bytes at 4096 after the run.
#define MARKER_ADDR 4096

// Write `s` to the marker region. Max 63 bytes + NUL.
static inline void mark(const char *s) {
    char *p = (char *)(intptr_t)MARKER_ADDR;
    for (int i = 0; s[i] && i < 63; i++) {
        p[i] = s[i];
    }
    p[63] = 0;
}

static inline void mark_pass(void) { mark("PASS"); }
static inline void mark_fail(const char *reason) {
    char buf[64];
    int i = 0;
    for (; i < 5 && "FAIL:"[i]; i++) buf[i] = "FAIL:"[i];
    for (int j = 0; reason[j] && i < 63; j++, i++) buf[i] = reason[j];
    buf[i] = 0;
    mark(buf);
}

// Convenience wrappers for syscalls taking 1..6 args.
static inline int64_t sc1(int64_t nr, int64_t a) {
    return __kernel_syscall(nr, a, 0, 0, 0, 0, 0);
}
static inline int64_t sc2(int64_t nr, int64_t a, int64_t b) {
    return __kernel_syscall(nr, a, b, 0, 0, 0, 0);
}
static inline int64_t sc3(int64_t nr, int64_t a, int64_t b, int64_t c) {
    return __kernel_syscall(nr, a, b, c, 0, 0, 0);
}
static inline int64_t sc4(int64_t nr, int64_t a, int64_t b, int64_t c, int64_t d) {
    return __kernel_syscall(nr, a, b, c, d, 0, 0);
}
static inline int64_t sc5(int64_t nr, int64_t a1, int64_t a2, int64_t a3,
                          int64_t a4, int64_t a5) {
    return __kernel_syscall(nr, a1, a2, a3, a4, a5, 0);
}
static inline int64_t sc6(int64_t nr, int64_t a1, int64_t a2, int64_t a3,
                          int64_t a4, int64_t a5, int64_t a6) {
    return __kernel_syscall(nr, a1, a2, a3, a4, a5, a6);
}

#endif // EDGE_LIBOS_CONFORMANCE_SYSCALL_H