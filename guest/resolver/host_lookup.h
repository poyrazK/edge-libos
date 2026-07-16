/*
 * host_lookup.h — guest-side wrapper around the project-private
 * NR_RESOLVE syscall (NR 400, see ADR 0007).
 *
 * Provided as a static inline so each translation unit that includes
 * it emits its own thin call — no symbol-table footprint, no link
 * order trap, no extra .o file beyond what the includer already is.
 *
 * Wire contract (matches src/sys/resolver.rs::resolve):
 *   a[0] = node_ptr      (u32 guest ptr; 0 = no node)
 *   a[1] = node_len      (i64; 0 = scan to NUL; cap 256)
 *   a[2] = service_ptr   (u32 guest ptr; 0 = no service)
 *   a[3] = service_len   (i64; 0 = scan to NUL; cap 64)
 *   a[4] = hints_ptr     (u32 guest ptr; 0 = no hints)
 *   a[5] = res_ptr_ptr   (u32 guest ptr to a u32 slot; handler writes head)
 *
 * Return: >= 0 success (count of addrinfo nodes written),
 *         <  0 -EAI_* (musl-negative: -1 BADFLAGS, -2 NONAME, ...).
 *
 * The caller (getaddrinfo.c below) is responsible for putting the
 * `node`, `service`, `hints`, and `res` slots in valid guest memory
 * and for walking the linked list returned at *res_ptr_ptr.
 */

#ifndef EDGE_LIBOS_GUEST_RESOLVER_HOST_LOOKUP_H
#define EDGE_LIBOS_GUEST_RESOLVER_HOST_LOOKUP_H

#include <stdint.h>

/* Re-declare the kernel.syscall import here rather than #include a
 * separate header, because the project's syscall_shim does the same
 * in musl_syscall.c — there's no shared `<edge_syscall.h>` yet.
 * This declaration MUST match src/bin/edge_cli.rs's add_to_linker
 * registration of (import "kernel" "syscall") and the per-arg
 * convention (nr + 6 i64 args). */
__attribute__((import_module("kernel"), import_name("syscall")))
int64_t __kernel_syscall(int64_t nr, int64_t a1, int64_t a2, int64_t a3,
                         int64_t a4, int64_t a5, int64_t a6);

/* Project-private syscall number. Must stay in sync with
 * src/sys/resolver.rs::NR_RESOLVE and tests/conformance/syscall.h. */
#define NR_RESOLVE 400

/*
 * Edge host lookup. `node_ptr`/`service_ptr`/`hints_ptr` are
 * u32 guest pointers; the host's mem layer will reject anything that
 * doesn't fall inside linear memory with -EFAULT (returned as -EAI_SYSTEM
 * after the from-EAI translation in getaddrinfo.c).
 *
 * `res_ptr_ptr` must point at a u32 slot in guest memory — the host
 * writes the head pointer of the marshalled addrinfo linked list to
 * that slot on success.
 */
static inline int64_t edge_host_lookup(uint32_t node_ptr, int64_t node_len,
                                       uint32_t service_ptr, int64_t service_len,
                                       uint32_t hints_ptr, uint32_t res_ptr_ptr) {
    return __kernel_syscall(
        (int64_t)NR_RESOLVE,
        (int64_t)node_ptr, node_len,
        (int64_t)service_ptr, service_len,
        (int64_t)hints_ptr, (int64_t)res_ptr_ptr);
}

#endif /* EDGE_LIBOS_GUEST_RESOLVER_HOST_LOOKUP_H */