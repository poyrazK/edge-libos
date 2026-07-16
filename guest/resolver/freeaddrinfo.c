/*
 * freeaddrinfo.c — musl override counterpart to getaddrinfo.c.
 *
 * Walks the malloc'd addrinfo list (whose nodes we built in
 * getaddrinfo.c), freeing each ai_addr payload and then the node
 * itself. Standard POSIX semantics.
 *
 * Defined in its own .o so a guest that pulls in getaddrinfo.c via
 * a different translation unit still gets the matching free.
 *
 * No libc headers — we declare just the types we need. The full
 * malloc/free declarations and the addrinfo struct come from
 * <stdlib.h> / <netdb.h> at link time when this .o is paired with
 * musl; for our purposes here we forward-declare just enough to
 * compile under wasm32-freestanding.
 */

#include <stdint.h>
#include <stddef.h>

/* Forward declarations to avoid pulling in musl headers at this
 * translation unit — the symbols themselves are resolved at link
 * time from libpython's bundled libc. */
extern void free(void *ptr);

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

void freeaddrinfo(struct edge_addrinfo *res) {
    while (res != NULL) {
        struct edge_addrinfo *next = (struct edge_addrinfo *)(uintptr_t)res->ai_next;
        free((void *)(uintptr_t)res->ai_addr);
        free((void *)(uintptr_t)res->ai_canonname);
        free(res);
        res = next;
    }
}