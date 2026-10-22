/*
 * libbpshim.so: LD_PRELOAD shim that makes socket(AF_BP, SOCK_DGRAM, ...) work
 * without the bp-socket kernel module.
 *
 * Every wrapper looks the fd up in a table of BP sockets. Fds that are not in
 * the table (every ordinary socket, file, pipe...) are forwarded untouched to the
 * real libc function found with dlsym(RTLD_NEXT).
 *
 * A BP socket is a real eventfd, which reserves a valid, unique fd number; all of
 * its behaviour lives in libbp (Rust) behind the opaque BpSocket handle.
 *
 * read()/write() on a BP fd act as recv()/send(). Not supported (see README): poll/epoll
 * (they see only the placeholder eventfd and never fire), threads sharing a BP fd, dup, BP flags.
 */
#define _GNU_SOURCE
#include <dlfcn.h>
#include <errno.h>
#include <limits.h>
#include <stdarg.h>
#include <stdatomic.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/eventfd.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/time.h>
#include <sys/types.h>
#include <sys/uio.h>
#include <unistd.h>

#include "bp.h"
#include "bp_socket.h"

#define SHIM_EXPORT __attribute__((visibility("default")))

/* ---- per-socket state -------------------------------------------------- */

struct bp_row {
  BpSocket *h;              /* libbp handle */
  pid_t pid;                /* process that created it (fork children only borrow it) */
  dev_t dev;                /* identity of the placeholder eventfd, to notice if the fd was */
  ino_t ino;                /* replaced behind our back (dup2, close_range, ...) */
  mode_t mode;
  int protocol;             /* value passed to socket(), reported by SO_PROTOCOL */
  int nonblock;             /* created with SOCK_NONBLOCK */
  struct sockaddr_bp local; /* valid when bound */
  int bound;
  struct sockaddr_bp peer;  /* default destination, valid when has_peer (connect) */
  int has_peer;
  int rcv_ms;               /* SO_RCVTIMEO in ms; -1 = wait forever */
  struct timeval rcv_tv;    /* as set by the app, for getsockopt */
  struct timeval snd_tv;    /* SO_SNDTIMEO is stored but never enforced */
};

/* Fixed, lock-free table indexed by fd. Wrapping read()/write() puts a lookup on every such
 * call in the program, including from signal handlers, so it must not take a lock.
 * Slot ownership is simple: only the creator of an fd number stores to it, and whoever
 * removes a row (close or stale detection) gets it back from the exchange. */
#define MAX_FDS 65536
static _Atomic(struct bp_row *) table[MAX_FDS];

static void row_release(struct bp_row *r);

/* Returns the BP row for fd, or NULL if fd is not (or no longer) one of ours.
 * Anything that closes or replaces the fd without going through close() (dup2,
 * close_range, libc closing internally) would leave a stale row that could hijack
 * whatever reuses the number, so the fd is checked against the eventfd we created.
 * (All eventfds share one inode, so this cannot tell two eventfds apart; it does
 * catch the fd turning into a file, socket, pipe, ... or being closed.) */
static struct bp_row *row_get(int fd) {
  if (fd < 0 || fd >= MAX_FDS)
    return NULL;
  struct bp_row *r = atomic_load(&table[fd]);
  if (!r)
    return NULL;
  int saved = errno;
  struct stat st;
  if (fstat(fd, &st) == 0 && st.st_dev == r->dev && st.st_ino == r->ino &&
      (st.st_mode & S_IFMT) == (r->mode & S_IFMT)) {
    errno = saved;
    return r;
  }
  if (atomic_compare_exchange_strong(&table[fd], &r, NULL))
    row_release(r);
  errno = saved;
  return NULL;
}

static int row_put(int fd, struct bp_row *r) {
  if (fd >= MAX_FDS) {
    errno = EMFILE;
    return -1;
  }
  atomic_store(&table[fd], r);
  return 0;
}

static struct bp_row *row_take(int fd) {
  if (fd < 0 || fd >= MAX_FDS)
    return NULL;
  return atomic_exchange(&table[fd], NULL);
}

/* ---- real libc functions ----------------------------------------------- */

static int (*real_socket)(int, int, int);
static int (*real_bind)(int, const struct sockaddr *, socklen_t);
static int (*real_connect)(int, const struct sockaddr *, socklen_t);
static int (*real_listen)(int, int);
static int (*real_accept)(int, struct sockaddr *, socklen_t *);
static int (*real_accept4)(int, struct sockaddr *, socklen_t *, int);
static ssize_t (*real_send)(int, const void *, size_t, int);
static ssize_t (*real_sendto)(int, const void *, size_t, int, const struct sockaddr *, socklen_t);
static ssize_t (*real_sendmsg)(int, const struct msghdr *, int);
static ssize_t (*real_recv)(int, void *, size_t, int);
static ssize_t (*real_recvfrom)(int, void *, size_t, int, struct sockaddr *, socklen_t *);
static ssize_t (*real_recvmsg)(int, struct msghdr *, int);
static int (*real_setsockopt)(int, int, int, const void *, socklen_t);
static int (*real_getsockopt)(int, int, int, void *, socklen_t *);
static int (*real_getsockname)(int, struct sockaddr *, socklen_t *);
static int (*real_getpeername)(int, struct sockaddr *, socklen_t *);
static int (*real_shutdown)(int, int);
static int (*real_close)(int);
static ssize_t (*real_read)(int, void *, size_t);
static ssize_t (*real_write)(int, const void *, size_t);

static int resolved;

static void resolve_all(void) {
#define R(name) real_##name = dlsym(RTLD_NEXT, #name)
  R(socket); R(bind); R(connect); R(listen); R(accept); R(accept4);
  R(send); R(sendto); R(sendmsg); R(recv); R(recvfrom); R(recvmsg);
  R(setsockopt); R(getsockopt); R(getsockname); R(getpeername);
  R(shutdown); R(close); R(read); R(write);
#undef R
  resolved = 1;
}

/* Resolve at load time so the first wrapped call never has to run dlsym (which allocates)
 * from inside a signal handler. ENSURE() remains as a fallback for very early callers. */
__attribute__((constructor)) static void shim_init(void) { resolve_all(); }

/* Idempotent, so a benign race between threads is harmless. */
#define ENSURE() do { if (!resolved) resolve_all(); } while (0)

/* BP_SHIM_DEBUG=1 traces BP calls on stderr. */
static int debug_on(void) {
  static int state = -1;
  if (state < 0) {
    const char *v = getenv("BP_SHIM_DEBUG");
    state = (v && *v && *v != '0');
  }
  return state;
}
#define DBG(...) do { if (debug_on()) { fprintf(stderr, "[bpshim] " __VA_ARGS__); fputc('\n', stderr); } } while (0)

/* Drops a row whose fd is gone: release the endpoint (only in the process that owns it;
 * a forked child just drops its copy) and free it. */
static void row_release(struct bp_row *r) {
  if (r->pid == getpid())
    bp_close(r->h);
  free(r);
}

/* ---- helpers ----------------------------------------------------------- */

static void make_addr(struct sockaddr_bp *a, uint32_t node, uint32_t service) {
  memset(a, 0, sizeof(*a));
  a->bp_family = AF_BP;
  a->bp_scheme = BP_SCHEME_IPN;
  a->bp_addr.ipn.node_id = node;
  a->bp_addr.ipn.service_id = service;
}

/* Validates an address supplied by the app; only ipn: is supported. */
static int parse_addr(const struct sockaddr *sa, socklen_t len, struct sockaddr_bp *out) {
  if (!sa || len < sizeof(struct sockaddr_bp)) {
    errno = EINVAL;
    return -1;
  }
  memcpy(out, sa, sizeof(*out));
  if (out->bp_family != AF_BP || out->bp_scheme != BP_SCHEME_IPN) {
    errno = EAFNOSUPPORT;
    return -1;
  }
  return 0;
}

/* Copies an address to an app buffer the way the kernel does: truncate, but
 * report the real size in *len. */
static void put_addr(const struct sockaddr_bp *a, struct sockaddr *dst, socklen_t *len) {
  if (!dst || !len)
    return;
  socklen_t n = *len < sizeof(*a) ? *len : (socklen_t)sizeof(*a);
  memcpy(dst, a, n);
  *len = sizeof(*a);
}

static int timeval_to_ms(const struct timeval *tv) {
  long long ms = (long long)tv->tv_sec * 1000 + (tv->tv_usec + 999) / 1000;
  if (ms == 0)
    return -1; /* zero means no timeout, as on real sockets */
  return ms > INT_MAX ? INT_MAX : (int)ms;
}

/* Gathers an iovec into one malloc'd buffer. */
static void *flatten(const struct iovec *iov, size_t cnt, size_t *total) {
  size_t n = 0;
  for (size_t i = 0; i < cnt; i++) {
    if (iov[i].iov_len > SIZE_MAX - n) {
      errno = EMSGSIZE;
      return NULL;
    }
    n += iov[i].iov_len;
  }
  char *buf = malloc(n ? n : 1);
  if (!buf) {
    errno = ENOMEM;
    return NULL;
  }
  size_t off = 0;
  for (size_t i = 0; i < cnt; i++) {
    if (iov[i].iov_len)
      memcpy(buf + off, iov[i].iov_base, iov[i].iov_len);
    off += iov[i].iov_len;
  }
  *total = n;
  return buf;
}

/* ---- send / receive cores ---------------------------------------------- */

static ssize_t bp_send(struct bp_row *r, const void *buf, size_t len,
                       const struct sockaddr *dest, socklen_t dlen) {
  struct sockaddr_bp to;
  if (dest) {
    if (parse_addr(dest, dlen, &to) < 0)
      return -1;
  } else if (r->has_peer) {
    to = r->peer;
  } else {
    errno = EDESTADDRREQ;
    return -1;
  }
  DBG("send %zu bytes to ipn:%u.%u", len, to.bp_addr.ipn.node_id, to.bp_addr.ipn.service_id);
  /* Flags (priority, custody, ...) are out of scope and ignored. */
  return bp_sendto(r->h, buf, len, to.bp_addr.ipn.node_id, to.bp_addr.ipn.service_id);
}

/* Returns bytes copied; *full is the real payload size so callers can flag truncation. */
static ssize_t bp_recv(struct bp_row *r, void *buf, size_t len, int flags,
                       struct sockaddr_bp *src, size_t *full) {
  if (flags & MSG_PEEK) {
    errno = EOPNOTSUPP;
    return -1;
  }
  int timeout = (r->nonblock || (flags & MSG_DONTWAIT)) ? 0 : r->rcv_ms;
  uint32_t node = 0, service = 0;
  size_t real = 0;
  ssize_t got = bp_recvfrom(r->h, buf, len, timeout, &node, &service, &real);
  if (got < 0)
    return -1;
  make_addr(src, node, service);
  *full = real;
  DBG("recv %zd bytes (of %zu) from ipn:%u.%u", got, real, node, service);
  return got;
}

/* ---- wrappers ---------------------------------------------------------- */

SHIM_EXPORT int socket(int domain, int type, int protocol) {
  ENSURE();
  if (domain != AF_BP)
    return real_socket(domain, type, protocol);

  if ((type & ~(SOCK_CLOEXEC | SOCK_NONBLOCK)) != SOCK_DGRAM) {
    errno = ESOCKTNOSUPPORT;
    return -1;
  }
  if (protocol != 0 && protocol != 1) { /* receiver.c passes 1, sender.c passes 0 */
    errno = EPROTONOSUPPORT;
    return -1;
  }

  /* The eventfd carries no data; it only reserves a valid fd number. */
  int fd = eventfd(0, ((type & SOCK_CLOEXEC) ? EFD_CLOEXEC : 0) |
                          ((type & SOCK_NONBLOCK) ? EFD_NONBLOCK : 0));
  if (fd < 0)
    return -1;

  struct bp_row *r = calloc(1, sizeof(*r));
  struct stat st;
  if (!r || fstat(fd, &st) < 0) {
    int e = r ? errno : ENOMEM;
    free(r);
    real_close(fd);
    errno = e;
    return -1;
  }
  r->pid = getpid();
  r->dev = st.st_dev;
  r->ino = st.st_ino;
  r->mode = st.st_mode;
  r->protocol = protocol;
  r->nonblock = (type & SOCK_NONBLOCK) != 0;
  r->rcv_ms = -1;
  r->h = bp_socket(NULL);
  if (!r->h) {
    int e = errno;
    free(r);
    real_close(fd);
    errno = e;
    return -1;
  }
  if (row_put(fd, r) < 0) {
    int e = errno;
    bp_close(r->h);
    free(r);
    real_close(fd);
    errno = e;
    return -1;
  }
  DBG("socket -> fd %d", fd);
  return fd;
}

SHIM_EXPORT int bind(int fd, const struct sockaddr *addr, socklen_t len) {
  ENSURE();
  struct bp_row *r = row_get(fd);
  if (!r)
    return real_bind(fd, addr, len);
  struct sockaddr_bp a;
  if (parse_addr(addr, len, &a) < 0)
    return -1;
  if (bp_bind(r->h, a.bp_addr.ipn.node_id, a.bp_addr.ipn.service_id) < 0)
    return -1;
  r->local = a;
  r->bound = 1;
  DBG("fd %d bound to ipn:%u.%u", fd, a.bp_addr.ipn.node_id, a.bp_addr.ipn.service_id);
  return 0;
}

SHIM_EXPORT int connect(int fd, const struct sockaddr *addr, socklen_t len) {
  ENSURE();
  struct bp_row *r = row_get(fd);
  if (!r)
    return real_connect(fd, addr, len);
  /* Datagram semantics, like UDP: remember a default destination, send nothing. */
  if (addr && len >= sizeof(sa_family_t) && addr->sa_family == AF_UNSPEC) {
    r->has_peer = 0;
    return 0;
  }
  struct sockaddr_bp a;
  if (parse_addr(addr, len, &a) < 0)
    return -1;
  r->peer = a;
  r->has_peer = 1;
  return 0;
}

SHIM_EXPORT int listen(int fd, int backlog) {
  ENSURE();
  if (row_get(fd)) {
    errno = EOPNOTSUPP;
    return -1;
  }
  return real_listen(fd, backlog);
}

SHIM_EXPORT int accept(int fd, struct sockaddr *addr, socklen_t *len) {
  ENSURE();
  if (row_get(fd)) {
    errno = EOPNOTSUPP;
    return -1;
  }
  return real_accept(fd, addr, len);
}

SHIM_EXPORT int accept4(int fd, struct sockaddr *addr, socklen_t *len, int flags) {
  ENSURE();
  if (row_get(fd)) {
    errno = EOPNOTSUPP;
    return -1;
  }
  return real_accept4(fd, addr, len, flags);
}

SHIM_EXPORT ssize_t sendto(int fd, const void *buf, size_t len, int flags,
                           const struct sockaddr *dest, socklen_t dlen) {
  ENSURE();
  struct bp_row *r = row_get(fd);
  if (!r)
    return real_sendto(fd, buf, len, flags, dest, dlen);
  return bp_send(r, buf, len, dest, dlen);
}

SHIM_EXPORT ssize_t send(int fd, const void *buf, size_t len, int flags) {
  ENSURE();
  struct bp_row *r = row_get(fd);
  if (!r)
    return real_send(fd, buf, len, flags);
  return bp_send(r, buf, len, NULL, 0);
}

SHIM_EXPORT ssize_t sendmsg(int fd, const struct msghdr *msg, int flags) {
  ENSURE();
  struct bp_row *r = row_get(fd);
  if (!r)
    return real_sendmsg(fd, msg, flags);
  size_t total = 0;
  void *buf = flatten(msg->msg_iov, msg->msg_iovlen, &total);
  if (!buf)
    return -1;
  ssize_t n = bp_send(r, buf, total, msg->msg_name, msg->msg_namelen);
  int e = errno;
  free(buf);
  errno = e;
  return n;
}

SHIM_EXPORT ssize_t recvfrom(int fd, void *buf, size_t len, int flags,
                             struct sockaddr *src, socklen_t *slen) {
  ENSURE();
  struct bp_row *r = row_get(fd);
  if (!r)
    return real_recvfrom(fd, buf, len, flags, src, slen);
  struct sockaddr_bp from;
  size_t full = 0;
  ssize_t n = bp_recv(r, buf, len, flags, &from, &full);
  if (n < 0)
    return -1;
  put_addr(&from, src, slen);
  return (flags & MSG_TRUNC) ? (ssize_t)full : n;
}

SHIM_EXPORT ssize_t recv(int fd, void *buf, size_t len, int flags) {
  ENSURE();
  struct bp_row *r = row_get(fd);
  if (!r)
    return real_recv(fd, buf, len, flags);
  struct sockaddr_bp from;
  size_t full = 0;
  ssize_t n = bp_recv(r, buf, len, flags, &from, &full);
  if (n < 0)
    return -1;
  return (flags & MSG_TRUNC) ? (ssize_t)full : n;
}

SHIM_EXPORT ssize_t recvmsg(int fd, struct msghdr *msg, int flags) {
  ENSURE();
  struct bp_row *r = row_get(fd);
  if (!r)
    return real_recvmsg(fd, msg, flags);

  size_t cap = 0;
  for (size_t i = 0; i < msg->msg_iovlen; i++)
    cap += msg->msg_iov[i].iov_len;

  /* One iovec can be filled directly; several need a bounce buffer. */
  void *bounce = NULL;
  void *dst = msg->msg_iovlen == 1 ? msg->msg_iov[0].iov_base : NULL;
  if (msg->msg_iovlen > 1) {
    bounce = malloc(cap ? cap : 1);
    if (!bounce) {
      errno = ENOMEM;
      return -1;
    }
    dst = bounce;
  }

  struct sockaddr_bp from;
  size_t full = 0;
  ssize_t n = bp_recv(r, dst, cap, flags, &from, &full);
  if (n < 0) {
    int e = errno;
    free(bounce);
    errno = e;
    return -1;
  }
  if (bounce) {
    size_t off = 0;
    for (size_t i = 0; i < msg->msg_iovlen && off < (size_t)n; i++) {
      size_t chunk = msg->msg_iov[i].iov_len;
      if (chunk > (size_t)n - off)
        chunk = (size_t)n - off;
      memcpy(msg->msg_iov[i].iov_base, (char *)bounce + off, chunk);
      off += chunk;
    }
    free(bounce);
  }

  if (msg->msg_name) {
    socklen_t l = msg->msg_namelen;
    put_addr(&from, msg->msg_name, &l);
    msg->msg_namelen = l;
  } else {
    msg->msg_namelen = 0;
  }
  msg->msg_controllen = 0;
  msg->msg_flags = full > (size_t)n ? MSG_TRUNC : 0;
  return (flags & MSG_TRUNC) ? (ssize_t)full : n;
}

SHIM_EXPORT int setsockopt(int fd, int level, int opt, const void *val, socklen_t len) {
  ENSURE();
  struct bp_row *r = row_get(fd);
  if (!r)
    return real_setsockopt(fd, level, opt, val, len);
  if (level == SOL_SOCKET && (opt == SO_RCVTIMEO || opt == SO_SNDTIMEO)) {
    struct timeval tv;
    if (!val || len < sizeof(tv)) {
      errno = EINVAL;
      return -1;
    }
    memcpy(&tv, val, sizeof(tv));
    if (tv.tv_sec < 0 || tv.tv_usec < 0 || tv.tv_usec >= 1000000) {
      errno = EDOM;
      return -1;
    }
    if (opt == SO_RCVTIMEO) {
      r->rcv_tv = tv;
      r->rcv_ms = timeval_to_ms(&tv);
      DBG("fd %d receive timeout %d ms", fd, r->rcv_ms);
    } else {
      r->snd_tv = tv;
    }
    return 0;
  }
  /* Other socket-level options (buffers, reuse flags, ...) have no BP meaning here; accept
   * them so apps that tune them keep working. Other levels (IP, TCP, ...) do not exist. */
  if (level != SOL_SOCKET) {
    errno = ENOPROTOOPT;
    return -1;
  }
  return 0;
}

SHIM_EXPORT int getsockopt(int fd, int level, int opt, void *val, socklen_t *len) {
  ENSURE();
  struct bp_row *r = row_get(fd);
  if (!r)
    return real_getsockopt(fd, level, opt, val, len);
  if (level != SOL_SOCKET || !val || !len) {
    errno = (level == SOL_SOCKET) ? EFAULT : ENOPROTOOPT;
    return -1;
  }
  int ival;
  const void *src;
  size_t sz;
  switch (opt) {
  case SO_RCVTIMEO: src = &r->rcv_tv; sz = sizeof(r->rcv_tv); break;
  case SO_SNDTIMEO: src = &r->snd_tv; sz = sizeof(r->snd_tv); break;
  case SO_TYPE:     ival = SOCK_DGRAM;     src = &ival; sz = sizeof(ival); break;
  case SO_DOMAIN:   ival = AF_BP;          src = &ival; sz = sizeof(ival); break;
  case SO_PROTOCOL: ival = r->protocol;    src = &ival; sz = sizeof(ival); break;
  case SO_ERROR:    ival = 0;              src = &ival; sz = sizeof(ival); break;
  default:
    errno = ENOPROTOOPT;
    return -1;
  }
  size_t n = *len < sz ? *len : sz;
  memcpy(val, src, n);
  *len = (socklen_t)n;
  return 0;
}

SHIM_EXPORT int getsockname(int fd, struct sockaddr *addr, socklen_t *len) {
  ENSURE();
  struct bp_row *r = row_get(fd);
  if (!r)
    return real_getsockname(fd, addr, len);
  struct sockaddr_bp a;
  if (r->bound) {
    a = r->local;
  } else {
    memset(&a, 0, sizeof(a));
    a.bp_family = AF_BP;
  }
  put_addr(&a, addr, len);
  return 0;
}

SHIM_EXPORT int getpeername(int fd, struct sockaddr *addr, socklen_t *len) {
  ENSURE();
  struct bp_row *r = row_get(fd);
  if (!r)
    return real_getpeername(fd, addr, len);
  if (!r->has_peer) {
    errno = ENOTCONN;
    return -1;
  }
  put_addr(&r->peer, addr, len);
  return 0;
}

SHIM_EXPORT int shutdown(int fd, int how) {
  ENSURE();
  if (row_get(fd))
    return 0; /* datagram sockets have no stream to shut down */
  return real_shutdown(fd, how);
}

SHIM_EXPORT ssize_t read(int fd, void *buf, size_t len) {
  ENSURE();
  struct bp_row *r = row_get(fd);
  if (!r)
    return real_read(fd, buf, len);
  struct sockaddr_bp from;
  size_t full = 0;
  return bp_recv(r, buf, len, 0, &from, &full);
}

SHIM_EXPORT ssize_t write(int fd, const void *buf, size_t len) {
  ENSURE();
  struct bp_row *r = row_get(fd);
  if (!r)
    return real_write(fd, buf, len);
  return bp_send(r, buf, len, NULL, 0);
}

SHIM_EXPORT int close(int fd) {
  ENSURE();
  struct bp_row *r = row_take(fd);
  if (!r)
    return real_close(fd);

  /* bp_close() may change errno; a successful close must leave it alone. */
  int saved = errno;
  if (r->pid == getpid()) {
    DBG("close fd %d", fd);
    bp_close(r->h); /* best effort: the node may already be gone */
  }
  /* else: a forked child only drops its copy; the parent still owns the endpoint. */
  free(r);
  errno = saved;
  return real_close(fd);
}

/* Unregister endpoints of sockets the program never closed. */
__attribute__((destructor)) static void shim_exit(void) {
  pid_t me = getpid();
  for (size_t i = 0; i < MAX_FDS; i++) {
    struct bp_row *r = atomic_load(&table[i]);
    if (r && r->pid == me && r->h) {
      bp_close(r->h);
      r->h = NULL;
    }
  }
}
