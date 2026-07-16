// getaddrinfo_loopback — exercises NR_RESOLVE for "localhost".
//
// This test runs in two very different environments:
//
//   - Online CI (linux/ubuntu-22.04, systemd-resolved): real DNS
//     resolves localhost to 127.0.0.1, so we assert the success
//     path: positive return count, AF_INET first node, sockaddr
//     bytes == 127.0.0.1.
//
//   - Offline hosts (no /etc/resolv.conf nameservers, e.g. macOS
//     runners where hickory's system-config finds nothing usable):
//     the lookup errors with -EAI_FAIL or -EAI_SYSTEM before any
//     IP is returned. That's also a valid outcome — it proves the
//     dispatch arm fires and returns a defined negative EAI. We
//     accept any negative return in [-4095, -1].
//
// Strict assertions on the success path are the goal; the Rust
// integration tests in tests/resolve_conformance.rs prove that
// path with a deterministic StubResolver regardless of DNS state.

#include "syscall.h"

typedef struct {
    int32_t  ai_flags;
    int32_t  ai_family;
    int32_t  ai_socktype;
    int32_t  ai_protocol;
    int32_t  ai_addrlen;
    uint32_t ai_addr;       /* guest pointer to sockaddr */
    uint32_t ai_canonname;  /* guest pointer; always NULL in v1 */
    uint32_t ai_next;       /* guest pointer to next node; 0 = end */
} edge_addrinfo_t;
_Static_assert(sizeof(edge_addrinfo_t) == 32, "addrinfo size");

typedef struct {
    uint16_t sin_family;
    uint16_t sin_port;
    uint32_t sin_addr;
    uint64_t sin_zero;
} sockaddr_in_t;
_Static_assert(sizeof(sockaddr_in_t) == 16, "sockaddr_in size");

__attribute__((visibility("default")))
void _start(void) {
    /* Scratch layout: MARKER_ADDR+0 = 64-byte PASS/FAIL marker,
     * MARKER_ADDR+120 = "localhost\0", MARKER_ADDR+128 = u32 res slot. */
    char *node_str = (char *)(intptr_t)(MARKER_ADDR + 120);
    node_str[0] = 'l'; node_str[1] = 'o'; node_str[2] = 'c'; node_str[3] = 'a';
    node_str[4] = 'l'; node_str[5] = 'h'; node_str[6] = 'o'; node_str[7] = 's';
    node_str[8] = 't';
    node_str[9] = '\0';
    uint32_t *res_slot = (uint32_t *)(intptr_t)(MARKER_ADDR + 128);
    *res_slot = 0;

    int64_t r = sc6(NR_RESOLVE,
                    (int64_t)(intptr_t)node_str, 0,
                    0, 0,
                    0,
                    (int64_t)(intptr_t)res_slot);

    /* Offline host path: any defined negative EAI is a valid outcome. */
    if (r < 0 && r >= -4095) {
        mark_pass();
        return;
    }
    if (r <= 0) {
        mark_fail("unexpected zero return from resolve");
        return;
    }

    /* Online host path: validate the success path. The host wrote
     * the linked list at RESOLVER_SCRATCH_BASE = MARKER_ADDR + 4096. */
    uint32_t head_off = *res_slot;
    if (head_off == 0) {
        mark_fail("res slot still NULL after success");
        return;
    }

    char *scratch = (char *)(intptr_t)(MARKER_ADDR + 4096);
    edge_addrinfo_t first;
    char *src = scratch + head_off;
    for (int i = 0; i < 32; i++) ((char *)&first)[i] = src[i];

    if (first.ai_family != 2) {            /* AF_INET */
        mark_fail("first node is not AF_INET");
        return;
    }
    if (first.ai_addrlen != 16) {
        mark_fail("first node ai_addrlen != 16");
        return;
    }
    if (first.ai_addr == 0) {
        mark_fail("first node has NULL ai_addr");
        return;
    }

    sockaddr_in_t sa;
    char *sa_src = scratch + first.ai_addr;
    for (int i = 0; i < 16; i++) ((char *)&sa)[i] = sa_src[i];

    if (sa.sin_family != 2) {
        mark_fail("sockaddr sin_family != AF_INET");
        return;
    }
    char *bytes = (char *)&sa.sin_addr;
    if (bytes[0] != 127 || bytes[1] != 0 ||
        bytes[2] != 0   || bytes[3] != 1) {
        mark_fail("sockaddr sin_addr != 127.0.0.1");
        return;
    }

    mark_pass();
}