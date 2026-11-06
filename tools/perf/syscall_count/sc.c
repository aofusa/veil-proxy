// F-295: 1 要求あたりのシステムコールを数える LD_PRELOAD の小片（strace は遅くて負荷の形が変わる）。
// SIGUSR2 で SC_OUT（既定 /tmp/sc.txt）へ合計を書く。libc を経由する呼び出しだけを数える。
//
//   cc -O2 -shared -fPIC -o sc.so sc.c -ldl
//   SC_OUT=/tmp/sc.txt LD_PRELOAD=$PWD/sc.so ./server ... ; kill -USR2 <pid>
#define _GNU_SOURCE
#include <dlfcn.h>
#include <poll.h>
#include <signal.h>
#include <stdatomic.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/epoll.h>
#include <sys/mman.h>
#include <sys/socket.h>
#include <sys/uio.h>
#include <unistd.h>

enum { READ, SEND, WRITE, POLL, EPOLL_WAIT, EPOLL_CTL, WRITEV, SENDMSG, RECV, MMAP, MUNMAP, N };
static atomic_long c[N];
static const char *names[N] = {"read",    "send",   "write", "poll", "epoll_wait", "epoll_ctl",
                               "writev",  "sendmsg", "recv", "mmap", "munmap"};
#define COUNT(i) atomic_fetch_add_explicit(&c[i], 1, memory_order_relaxed)
#define NEXT(f) static __typeof__(&f) real; if (!real) real = dlsym(RTLD_NEXT, #f)

static void dump(int s) {
    (void)s;
    const char *p = getenv("SC_OUT");
    FILE *f = fopen(p ? p : "/tmp/sc.txt", "w");
    if (!f) return;
    for (int i = 0; i < N; i++) fprintf(f, "%s %ld\n", names[i], atomic_load(&c[i]));
    fclose(f);
}
__attribute__((constructor)) static void init(void) { signal(SIGUSR2, dump); }

ssize_t read(int fd, void *b, size_t n) { NEXT(read); COUNT(READ); return real(fd, b, n); }
ssize_t send(int fd, const void *b, size_t n, int fl) { NEXT(send); COUNT(SEND); return real(fd, b, n, fl); }
ssize_t write(int fd, const void *b, size_t n) { NEXT(write); COUNT(WRITE); return real(fd, b, n); }
int poll(struct pollfd *p, nfds_t n, int t) { NEXT(poll); COUNT(POLL); return real(p, n, t); }
int epoll_wait(int e, struct epoll_event *v, int m, int t) { NEXT(epoll_wait); COUNT(EPOLL_WAIT); return real(e, v, m, t); }
int epoll_ctl(int e, int o, int fd, struct epoll_event *v) { NEXT(epoll_ctl); COUNT(EPOLL_CTL); return real(e, o, fd, v); }
ssize_t writev(int fd, const struct iovec *v, int n) { NEXT(writev); COUNT(WRITEV); return real(fd, v, n); }
ssize_t sendmsg(int fd, const struct msghdr *m, int fl) { NEXT(sendmsg); COUNT(SENDMSG); return real(fd, m, fl); }
ssize_t recv(int fd, void *b, size_t n, int fl) { NEXT(recv); COUNT(RECV); return real(fd, b, n, fl); }
void *mmap(void *a, size_t l, int p, int fl, int fd, off_t o) { NEXT(mmap); COUNT(MMAP); return real(a, l, p, fl, fd, o); }
int munmap(void *a, size_t l) { NEXT(munmap); COUNT(MUNMAP); return real(a, l); }
