/* Private probe tripwire, NOT a sandbox/security guarantee. Every Cutex test
 * process gets an explicit numeric loopback port allowlist. Unexpected network
 * connect exits immediately before the syscall; Unix native RPC is allowed. */
#define _GNU_SOURCE
#include <arpa/inet.h>
#include <dlfcn.h>
#include <errno.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <unistd.h>

int connect(int fd, const struct sockaddr *addr, socklen_t len) {
    if (addr && (addr->sa_family == AF_INET || addr->sa_family == AF_INET6)) {
        unsigned port = 0;
        int local = 0;
        if (addr->sa_family == AF_INET && len >= sizeof(struct sockaddr_in)) {
            const struct sockaddr_in *a = (const struct sockaddr_in *)addr;
            port = ntohs(a->sin_port);
            local = ntohl(a->sin_addr.s_addr) == INADDR_LOOPBACK;
        } else if (addr->sa_family == AF_INET6 && len >= sizeof(struct sockaddr_in6)) {
            const struct sockaddr_in6 *a = (const struct sockaddr_in6 *)addr;
            port = ntohs(a->sin6_port);
            local = IN6_IS_ADDR_LOOPBACK(&a->sin6_addr);
        }
        const char *list = getenv("S4_TEST_ALLOWED_PORTS");
        int allowed = 0;
        while (local && list && *list) {
            char *end;
            unsigned long candidate = strtoul(list, &end, 10);
            if (end == list) break;
            if (candidate == port && (*end == ',' || !*end)) allowed = 1;
            if (*end != ',') break;
            list = end + 1;
        }
        if (!allowed) {
            const char msg[] = "S4_PROBE_UNOWNED_ENDPOINT_BLOCKED\n";
            (void)write(2, msg, sizeof(msg)-1);
            _exit(97);
        }
    }
    int (*real_connect)(int, const struct sockaddr *, socklen_t) = dlsym(RTLD_NEXT, "connect");
    if (!real_connect) { errno = EACCES; return -1; }
    return real_connect(fd, addr, len);
}
