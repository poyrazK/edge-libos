// getaddrinfo_eai_noname — exercises NR_RESOLVE for an unresolvable
// name ("this-host-does-not-exist.invalid"). The .invalid TLD is
// reserved by RFC 2606 to never resolve, so on a host with working
// DNS we expect a clean -EAI_NONAME return.
//
// This test runs in two very different environments:
//
//   - Online CI (linux/ubuntu-22.04, systemd-resolved): real DNS
//     resolves .invalid as NXDOMAIN → -EAI_NONAME.
//   - Offline hosts (no /etc/resolv.conf nameservers, e.g. macOS
//     runners where hickory's system-config finds nothing usable):
//     the lookup errors earlier with -EAI_FAIL or -EAI_SYSTEM.
//
// Both prove the syscall arm fires and returns a defined negative
// EAI value. We accept either. Strict -EAI_NONAME is the goal; the
// Rust integration tests in tests/resolve_conformance.rs prove that
// path with a deterministic StubResolver.

#include "syscall.h"

__attribute__((visibility("default")))
void _start(void) {
    char *node_str = (char *)(intptr_t)(MARKER_ADDR + 120);
    const char *src = "this-host-does-not-exist.invalid";
    for (int i = 0; src[i] != '\0'; i++) node_str[i] = src[i];
    node_str[33] = '\0';

    uint32_t *res_slot = (uint32_t *)(intptr_t)(MARKER_ADDR + 160);
    *res_slot = 0;

    int64_t r = sc6(NR_RESOLVE,
                    (int64_t)(intptr_t)node_str, 0,
                    0, 0,
                    0,
                    (int64_t)(intptr_t)res_slot);

    /* Accept any negative EAI_* return — both -2 (NONAME on real DNS)
     * and -4 (FAIL on offline host) prove the dispatch arm fires. */
    if (r < 0 && r >= -4095) {
        mark_pass();
        return;
    }
    mark_fail("expected negative EAI return, got >= 0");
}