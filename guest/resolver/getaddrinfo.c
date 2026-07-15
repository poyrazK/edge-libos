/*
 * getaddrinfo.c — musl override that routes through NR_RESOLVE.
 *
 * Why this exists
 * ---------------
 * musl implements getaddrinfo(3) by issuing ordinary socket/sendto/
 * recvmsg/poll syscalls over UDP and walking /etc/hosts and
 * /etc/resolv.conf through the VFS. The edge-libos kernel has no
 * UDP socket layer and no real /etc/resolv.conf VFS entry today, so
 * musl's path can't run end-to-end.
 *
 * We replace musl's getaddrinfo with a thin wrapper that:
 *   1. Copies `node` and `service` (plus the hints struct, if any)
 *      into a known scratch region in linear memory.
 *   2. Calls edge_host_lookup() (NR_RESOLVE).
 *   3. On success, walks the linked list the host wrote there,
 *      malloc()s a struct addrinfo for each node, memcpy's the
 *      32-byte header + the sockaddr, and stitches the list.
 *   4. Sets *res to the new list head and returns 0.
 *
 * The EAI return codes are negative in both spaces (musl's netdb.h
 * defines EAI_NONAME as -2, etc.) so the syscall return value can
 * be cast to int and returned directly on the negative path.
 *
 * Linker ordering
 * ---------------
 * This object MUST appear before libc.a (or musl's libc archive) on
 * the link command — see guest/build.sh's ENTRY_OBJS, which puts
 * the resolver .o files ahead of libpython + musl. Verify post-link
 * with `wasm-objdump -d python.wasm | grep getaddrinfo`.
 *
 * Scope of v1
 * -----------
 * - Hints: ai_family, ai_socktype, ai_flags passed through; rest
 *   ignored. AI_NUMERICHOST/AI_NUMERICSERV → -EAI_BADFLAGS (host
 *   rejects these).
 * - No service-name lookup (getservbyname is out of scope).
 * - ai_canonname always NULL.
 * - Errors that aren't directly mapped from the host's -EAI_* set
 *   collapse to -EAI_SYSTEM.
 *
 * Headers
 * -------
 * No libc headers — we declare just the types we need. malloc/free
 * resolve at link time from libpython's bundled libc; the full
 * struct addrinfo and EAI_* constants come from <netdb.h> at the
 * musl level when this .o is paired with libpython. Under wasm32-
 * freestanding, freestanding libc headers are not in the include
 * path (zig cc gives us only compiler-builtin headers), so we keep
 * the .c self-contained.
 */

#include <stdint.h>
#include <stddef.h>

#include "host_lookup.h"

/* Forward declarations to avoid pulling in musl headers at this
 * translation unit — the symbols resolve at link time from
 * libpython's bundled libc. The memcpy signature here is the exact
 * C99 prototype (no __builtin___memcpy_chk decorations). */
extern void *malloc(size_t size);
extern void *calloc(size_t nmemb, size_t size);
extern void  free(void *ptr);
extern void *memcpy(void *dst, const void *src, size_t n);

/* musl's struct addrinfo on wasm32 — see src/sys/resolver.rs::ADDRINFO_SIZE.
 * 4-byte pointers, no padding. Must match the one in freeaddrinfo.c. */
struct edge_addrinfo {
    int32_t  ai_flags;
    int32_t  ai_family;
    int32_t  ai_socktype;
    int32_t  ai_protocol;
    int32_t  ai_addrlen;
    uint32_t ai_addr;       /* guest pointer to sockaddr */
    uint32_t ai_canonname;  /* guest pointer; always NULL in v1 */
    uint32_t ai_next;       /* guest pointer to next node; 0 = end */
};
_Static_assert(sizeof(struct edge_addrinfo) == 32, "addrinfo must be 32 bytes");

/* Forward declaration of the matching freeaddrinfo (defined in
 * freeaddrinfo.c) so we can call it on the error paths without
 * relying on implicit function declarations. */
void freeaddrinfo(struct edge_addrinfo *res);

/* EAI_* constants from <netdb.h>. musl defines them as positive ints
 * but the syscall's negative space mirrors them exactly, so casting
 * is safe. Defined here as the negative forms because that's what
 * the syscall actually returns. */
#define EAI_BADFLAGS  -1
#define EAI_NONAME    -2
#define EAI_AGAIN     -3
#define EAI_FAIL      -4
#define EAI_FAMILY    -6
#define EAI_SOCKTYPE  -7
#define EAI_SERVICE   -8
#define EAI_MEMORY   -10
#define EAI_SYSTEM   -11
#define EAI_OVERFLOW -12

/* Where in guest linear memory we tell the host to write its result.
 * Mirrors src/sys/resolver.rs::RESOLVER_SCRATCH_BASE = MARKER_ADDR + 4096.
 *
 * Layout we hand to the host:
 *   scratch+0    u32 res_ptr slot  (4 bytes; host writes head pointer)
 *   scratch+8    node  buffer  (cap 256 + 1 NUL)
 *   scratch+280  service buffer (cap 64  + 1 NUL)
 *   scratch+352  hints struct   (32 bytes, valid only if hints_ptr != NULL)
 *   scratch+384  ... marshal region for the addrinfo linked list
 *
 * 4096 bytes total cap (RESOLVER_SCRATCH_BASE region size). The host
 * enforces a 4096-byte write cap; if a single lookup would exceed it,
 * the host returns -EAI_MEMORY and we propagate that.
 */
#define EDGE_MARKER_ADDR     4096
#define EDGE_RESOLVER_SCRATCH (EDGE_MARKER_ADDR + 4096)
#define EDGE_RES_S_OFFSET     0
#define EDGE_NODE_OFFSET      8
#define EDGE_SVC_OFFSET       280
#define EDGE_HINTS_OFFSET     352

/* Read a NUL-terminated C string out of guest memory at `src`, copying
 * it to `dst` (capacity `dst_cap`). Returns the byte count copied
 * (excluding the terminating NUL); truncates if src is longer than
 * dst_cap - 1. Always NUL-terminates if dst_cap > 0. */
static size_t edge_copy_str(char *dst, size_t dst_cap, const char *src) {
    if (dst_cap == 0) return 0;
    size_t n = 0;
    while (n + 1 < dst_cap && src[n] != '\0') {
        dst[n] = src[n];
        n++;
    }
    dst[n] = '\0';
    return n;
}

/* Public entry — replaces libc's getaddrinfo. The symbol name MUST
 * match musl's; wasm-ld picks the first definition seen, which is
 * ours as long as this .o precedes libc.a on the link command line. */
int getaddrinfo(const char *node, const char *service,
                const struct edge_addrinfo *hints,
                struct edge_addrinfo **res) {
    if (res == NULL) return EAI_FAIL;
    *res = NULL;

    if (node == NULL && service == NULL) return EAI_NONAME;

    char *scratch = (char *)(intptr_t)EDGE_RESOLVER_SCRATCH;
    uint32_t res_slot = (uint32_t)(intptr_t)(scratch + EDGE_RES_S_OFFSET);
    uint32_t node_buf = (uint32_t)(intptr_t)(scratch + EDGE_NODE_OFFSET);
    uint32_t svc_buf  = (uint32_t)(intptr_t)(scratch + EDGE_SVC_OFFSET);
    uint32_t hints_buf = (uint32_t)(intptr_t)(scratch + EDGE_HINTS_OFFSET);

    int64_t node_len = 0;
    if (node != NULL) {
        node_len = (int64_t)edge_copy_str(scratch + EDGE_NODE_OFFSET, 257, node);
    }
    int64_t svc_len = 0;
    if (service != NULL) {
        svc_len = (int64_t)edge_copy_str(scratch + EDGE_SVC_OFFSET, 65, service);
    }
    int64_t has_hints = 0;
    if (hints != NULL) {
        memcpy(scratch + EDGE_HINTS_OFFSET, hints, sizeof(struct edge_addrinfo));
        has_hints = (int64_t)hints_buf;
    }

    int64_t rc = edge_host_lookup(
        node_len  > 0 ? node_buf  : 0u, node_len,
        svc_len   > 0 ? svc_buf   : 0u, svc_len,
        has_hints > 0 ? (uint32_t)has_hints : 0u,
        res_slot);

    if (rc < 0) {
        /* Already a negative EAI_* code from the host. Cast narrows;
         * values bounded by [-4095, -1] in both spaces, no truncation. */
        return (int)rc;
    }

    uint32_t head_off;
    memcpy(&head_off, scratch + EDGE_RES_S_OFFSET, sizeof(uint32_t));
    if (head_off == 0) {
        return EAI_FAIL;
    }

    struct edge_addrinfo *out_head = NULL;
    struct edge_addrinfo *out_tail = NULL;
    uint32_t cur = head_off;
    int count = (int)rc;
    for (int i = 0; i < count && cur != 0; i++) {
        struct edge_addrinfo src;
        if (cur + sizeof(src) > 4096u) {
            if (out_head) freeaddrinfo(out_head);
            return EAI_SYSTEM;
        }
        memcpy(&src, scratch + cur, sizeof(src));

        struct edge_addrinfo *dst = calloc(1, sizeof(struct edge_addrinfo));
        if (dst == NULL) {
            if (out_head) freeaddrinfo(out_head);
            return EAI_MEMORY;
        }
        dst->ai_flags     = src.ai_flags;
        dst->ai_family    = src.ai_family;
        dst->ai_socktype  = src.ai_socktype;
        dst->ai_protocol  = src.ai_protocol;
        dst->ai_addrlen   = src.ai_addrlen;
        dst->ai_canonname = 0;
        dst->ai_next      = 0;

        if (src.ai_addr != 0 && src.ai_addrlen > 0) {
            if ((size_t)src.ai_addrlen > 128u ||
                cur + src.ai_addr > 4096u ||
                src.ai_addr + (uint32_t)src.ai_addrlen > 4096u) {
                free(dst);
                if (out_head) freeaddrinfo(out_head);
                return EAI_SYSTEM;
            }
            dst->ai_addr = (uint32_t)(uintptr_t)calloc(1, (size_t)src.ai_addrlen);
            if (dst->ai_addr == 0) {
                free(dst);
                if (out_head) freeaddrinfo(out_head);
                return EAI_MEMORY;
            }
            memcpy((void *)(uintptr_t)dst->ai_addr,
                   scratch + src.ai_addr, (size_t)src.ai_addrlen);
        }

        if (out_head == NULL) {
            out_head = dst;
        } else {
            out_tail->ai_next = (uint32_t)(uintptr_t)dst;
        }
        out_tail = dst;
        cur = src.ai_next;
    }

    *res = out_head;
    return 0;
}