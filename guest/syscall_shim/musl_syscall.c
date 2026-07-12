/*
 * musl_syscall.c — CPython syscall shim.
 *
 * musl's libc calls `__syscallN(arg1, ..., argN)` for N=0..6 to forward
 * to the kernel. We define those symbols here as thin wrappers around
 * `kernel.syscall`, which is our single import from the host.
 *
 * The mapping:
 *   __syscall0(ret_type)                       -> kernel.syscall(nr)
 *   __syscall1(ret_type, a1)                   -> kernel.syscall(nr, a1)
 *   __syscall2(ret_type, a1, a2)               -> kernel.syscall(nr, a1, a2)
 *   ...
 *   __syscall6(ret_type, a1..a6)               -> kernel.syscall(nr, a1..a6)
 *
 * In musl's convention, the *first* argument is the syscall number. This
 * matches our `(import "kernel" "syscall" (param i64 i64 ...))` signature
 * where params[0] is nr and params[1..7] are a1..a6.
 *
 * Build flags: --target=wasm32-freestanding -O2 -nostdlib
 *   (no libc; musl's libc is *replaced* by this shim when we cross-compile
 *   CPython against it).
 */

#include <stdint.h>

__attribute__((import_module("kernel"), import_name("syscall")))
int64_t __kernel_syscall(int64_t nr, int64_t a1, int64_t a2, int64_t a3,
                         int64_t a4, int64_t a5, int64_t a6);

/* musl-style syscall wrappers. Variadic returns are not used by CPython
 * in practice; we always pass through a single long return. Note that
 * on wasm32 `long` is 32-bit, so we explicitly use `int64_t` everywhere
 * to match the host's `kernel.syscall` import signature `(i64 x7) -> i64`.
 *
 * musl itself defines these as variadic with `long` (which is 64-bit on
 * x86_64 native), but when cross-compiled to wasm32-musl, musl's headers
 * redefine them via __SYSCALL_LL_E. The musl-side calls into these
 * trampolines carry the right types via the per-architecture syscall.h.
 */
int64_t __syscall0(int64_t nr) {
    return __kernel_syscall(nr, 0, 0, 0, 0, 0, 0);
}

int64_t __syscall1(int64_t nr, int64_t a1) {
    return __kernel_syscall(nr, a1, 0, 0, 0, 0, 0);
}

int64_t __syscall2(int64_t nr, int64_t a1, int64_t a2) {
    return __kernel_syscall(nr, a1, a2, 0, 0, 0, 0);
}

int64_t __syscall3(int64_t nr, int64_t a1, int64_t a2, int64_t a3) {
    return __kernel_syscall(nr, a1, a2, a3, 0, 0, 0);
}

int64_t __syscall4(int64_t nr, int64_t a1, int64_t a2, int64_t a3, int64_t a4) {
    return __kernel_syscall(nr, a1, a2, a3, a4, 0, 0);
}

int64_t __syscall5(int64_t nr, int64_t a1, int64_t a2, int64_t a3,
                   int64_t a4, int64_t a5) {
    return __kernel_syscall(nr, a1, a2, a3, a4, a5, 0);
}

int64_t __syscall6(int64_t nr, int64_t a1, int64_t a2, int64_t a3,
                   int64_t a4, int64_t a5, int64_t a6) {
    return __kernel_syscall(nr, a1, a2, a3, a4, a5, a6);
}

/*
 * musl also expects __syscall_cp variants for cancellation-point safety
 * (e.g. open, read, write, close). P0 single-threaded — no real cancel
 * state — so we forward to __kernel_syscall. CPython does not exercise
 * this path; we provide it for completeness so musl links cleanly.
 *
 * musl declares this as variadic with a struct pthread * hidden arg
 * slot, but the actual call site always passes 1..6 long args after
 * nr. We declare a fixed 6-arg signature that matches the dispatcher.
 */
struct pthread;

int64_t __syscall_cp(int (*fn)(void *), struct pthread *p, int64_t nr,
                     int64_t a1, int64_t a2, int64_t a3, int64_t a4,
                     int64_t a5, int64_t a6) {
    (void)fn;
    (void)p;
    return __kernel_syscall(nr, a1, a2, a3, a4, a5, a6);
}