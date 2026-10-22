/* Exercises libbpshim.so beyond what sender.c/receiver.c do. Run under LD_PRELOAD against a
 * dtn7-rs node ipn:10.0 (see tests/run_shim_test.sh); one node suffices because bundles
 * addressed to ipn:10.x are delivered locally. */
#define _GNU_SOURCE
#include "bp_socket.h"
#include <arpa/inet.h>
#include <errno.h>
#include <fcntl.h>
#include <netinet/in.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/time.h>
#include <sys/syscall.h>
#include <sys/uio.h>
#include <netinet/ip.h>
#include <sys/wait.h>
#include <unistd.h>

static int failures;
#define CHECK(cond, what)                                                      \
  do {                                                                         \
    if (cond) printf("  ok   %s\n", what);                                     \
    else { printf("  FAIL %s (errno=%d %s)\n", what, errno, strerror(errno)); failures++; } \
  } while (0)

static struct sockaddr_bp addr(uint32_t n, uint32_t s) {
  struct sockaddr_bp a;
  memset(&a, 0, sizeof a);
  a.bp_family = AF_BP;
  a.bp_scheme = BP_SCHEME_IPN;
  a.bp_addr.ipn.node_id = n;
  a.bp_addr.ipn.service_id = s;
  return a;
}
static const struct sockaddr *sa(const struct sockaddr_bp *a) { return (const struct sockaddr *)a; }

static double now(void) {
  struct timeval tv;
  gettimeofday(&tv, NULL);
  return tv.tv_sec + tv.tv_usec / 1e6;
}
static void set_timeout(int fd, long ms) {
  struct timeval tv = {ms / 1000, (ms % 1000) * 1000};
  setsockopt(fd, SOL_SOCKET, SO_RCVTIMEO, &tv, sizeof tv);
}
static void on_alarm(int sig) { (void)sig; }

static void udp_passthrough(const char *label) {
  int u = socket(AF_INET, SOCK_DGRAM, 0);
  struct sockaddr_in in = {.sin_family = AF_INET, .sin_addr.s_addr = htonl(INADDR_LOOPBACK)};
  socklen_t l = sizeof in;
  char buf[16] = {0};
  int ok = u >= 0 && bind(u, (struct sockaddr *)&in, sizeof in) == 0 &&
           getsockname(u, (struct sockaddr *)&in, &l) == 0 &&
           sendto(u, "ping", 4, 0, (struct sockaddr *)&in, sizeof in) == 4 &&
           recv(u, buf, sizeof buf, 0) == 4 && memcmp(buf, "ping", 4) == 0;
  CHECK(ok, label);
  if (u >= 0) close(u);
}

int main(void) {
  char buf[64];
  struct sockaddr_bp from, a1 = addr(10, 1), a2 = addr(10, 2);
  socklen_t fl;

  puts("passthrough");
  udp_passthrough("ordinary UDP socket still works");

  puts("socket()");
  CHECK(socket(AF_BP, SOCK_STREAM, 0) == -1 && errno == ESOCKTNOSUPPORT, "SOCK_STREAM refused");
  int recv_fd = socket(AF_BP, SOCK_DGRAM, 1);
  int send_fd = socket(AF_BP, SOCK_DGRAM, 0);
  CHECK(recv_fd >= 0 && send_fd >= 0 && recv_fd != send_fd, "two distinct BP fds");
  CHECK(fcntl(recv_fd, F_GETFD) != -1, "BP fd is a valid, open fd");
  CHECK(listen(recv_fd, 1) == -1 && errno == EOPNOTSUPP, "listen -> EOPNOTSUPP");
  CHECK(accept(recv_fd, NULL, NULL) == -1 && errno == EOPNOTSUPP, "accept -> EOPNOTSUPP");
  int so_type = 0; socklen_t tl = sizeof so_type;
  CHECK(getsockopt(recv_fd, SOL_SOCKET, SO_TYPE, &so_type, &tl) == 0 && so_type == SOCK_DGRAM,
        "SO_TYPE is SOCK_DGRAM");

  puts("bind / connect");
  CHECK(bind(recv_fd, sa(&a1), sizeof a1) == 0, "bind ipn:10.1");
  CHECK(bind(recv_fd, sa(&a2), sizeof a2) == -1 && errno == EINVAL, "second bind -> EINVAL");
  CHECK(bind(send_fd, sa(&a2), sizeof a2) == 0, "bind ipn:10.2");
  fl = sizeof from;
  CHECK(getsockname(recv_fd, (struct sockaddr *)&from, &fl) == 0 && from.bp_addr.ipn.service_id == 1,
        "getsockname returns the bound address");
  struct sockaddr_in wrong = {.sin_family = AF_INET};
  CHECK(sendto(send_fd, "x", 1, 0, (struct sockaddr *)&wrong, sizeof wrong) == -1 && errno == EAFNOSUPPORT,
        "non-BP destination -> EAFNOSUPPORT");
  CHECK(send(send_fd, "x", 1, 0) == -1 && errno == EDESTADDRREQ, "send without connect -> EDESTADDRREQ");
  CHECK(connect(send_fd, sa(&a1), sizeof a1) == 0, "connect stores default destination");
  fl = sizeof from;
  CHECK(getpeername(send_fd, (struct sockaddr *)&from, &fl) == 0 && from.bp_addr.ipn.service_id == 1,
        "getpeername returns it");

  puts("send / recv");
  CHECK(send(send_fd, "hello", 5, 0) == 5, "send() uses the default destination, returns length");
  set_timeout(recv_fd, 3000);
  fl = sizeof from;
  ssize_t n = recvfrom(recv_fd, buf, sizeof buf, 0, (struct sockaddr *)&from, &fl);
  CHECK(n == 5 && memcmp(buf, "hello", 5) == 0, "recvfrom gets the payload");
  CHECK(fl == sizeof from && from.bp_addr.ipn.node_id == 10 && from.bp_addr.ipn.service_id == 2,
        "recvfrom reports the sender ipn:10.2");

  CHECK(sendto(send_fd, "0123456789", 10, 0, sa(&a1), sizeof a1) == 10, "sendto 10 bytes");
  n = recv(recv_fd, buf, 4, MSG_TRUNC);
  CHECK(n == 10 && memcmp(buf, "0123", 4) == 0, "small buffer: 4 bytes copied, MSG_TRUNC returns real length");

  sendto(send_fd, "ABCDEFGH", 8, 0, sa(&a1), sizeof a1);
  char p1[3], p2[3];
  struct iovec iov[2] = {{p1, 3}, {p2, 3}};
  struct sockaddr_bp name;
  struct msghdr m = {.msg_name = &name, .msg_namelen = sizeof name, .msg_iov = iov, .msg_iovlen = 2};
  n = recvmsg(recv_fd, &m, 0);
  CHECK(n == 6 && memcmp(p1, "ABC", 3) == 0 && memcmp(p2, "DEF", 3) == 0, "recvmsg scatters into two iovecs");
  CHECK((m.msg_flags & MSG_TRUNC) != 0, "recvmsg sets MSG_TRUNC");
  CHECK(m.msg_namelen == sizeof name && name.bp_addr.ipn.service_id == 2, "recvmsg fills msg_name/msg_namelen");

  struct iovec out[2] = {{"foo", 3}, {"bar", 3}};
  struct msghdr sm = {.msg_name = (void *)&a1, .msg_namelen = sizeof a1, .msg_iov = out, .msg_iovlen = 2};
  CHECK(sendmsg(send_fd, &sm, 0) == 6, "sendmsg gathers two iovecs");
  n = recv(recv_fd, buf, sizeof buf, 0);
  CHECK(n == 6 && memcmp(buf, "foobar", 6) == 0, "...and they arrive joined");

  puts("read / write");
  CHECK(write(send_fd, "w", 1) == 1, "write() on a connected BP fd sends");
  n = read(recv_fd, buf, sizeof buf);
  CHECK(n == 1 && buf[0] == 'w', "read() on a BP fd receives");

  puts("setsockopt levels");
  int one = 1;
  CHECK(setsockopt(recv_fd, SOL_SOCKET, SO_REUSEADDR, &one, sizeof one) == 0, "SOL_SOCKET options are accepted");
  CHECK(setsockopt(recv_fd, IPPROTO_IP, IP_TTL, &one, sizeof one) == -1 && errno == ENOPROTOOPT,
        "options of other levels -> ENOPROTOOPT");

  puts("timeouts and signals");
  set_timeout(recv_fd, 300);
  struct timeval got; socklen_t gl = sizeof got;
  CHECK(getsockopt(recv_fd, SOL_SOCKET, SO_RCVTIMEO, &got, &gl) == 0 && got.tv_usec == 300000, "getsockopt reads back SO_RCVTIMEO");
  double t0 = now();
  n = recv(recv_fd, buf, sizeof buf, 0);
  double dt = now() - t0;
  CHECK(n == -1 && errno == EAGAIN && dt >= 0.28 && dt < 1.5, "empty queue: EAGAIN after ~300 ms");
  t0 = now();
  n = recv(recv_fd, buf, sizeof buf, MSG_DONTWAIT);
  CHECK(n == -1 && errno == EAGAIN && now() - t0 < 0.25, "MSG_DONTWAIT returns at once");

  struct sigaction act = {.sa_handler = on_alarm}; /* no SA_RESTART */
  sigaction(SIGALRM, &act, NULL);
  set_timeout(recv_fd, 0); /* 0 = wait forever */
  struct itimerval it = {.it_value = {0, 300000}};
  setitimer(ITIMER_REAL, &it, NULL);
  t0 = now();
  n = recv(recv_fd, buf, sizeof buf, 0);
  CHECK(n == -1 && errno == EINTR && now() - t0 < 2.0, "blocking recv is interrupted by a signal (EINTR)");

  puts("fork");
  set_timeout(recv_fd, 3000);
  pid_t pid = fork();
  if (pid == 0) { /* the child sends through libbp, then drops its copy of the receiver */
    int ok = sendto(send_fd, "kid", 3, 0, sa(&a1), sizeof a1) == 3;
    close(recv_fd);
    _exit(ok ? 0 : 1);
  }
  int st; waitpid(pid, &st, 0);
  CHECK(WIFEXITED(st) && WEXITSTATUS(st) == 0, "a forked child can send through libbp");
  n = recv(recv_fd, buf, sizeof buf, 0);
  CHECK(n == 3 && memcmp(buf, "kid", 3) == 0, "...and the parent receives it");
  sendto(send_fd, "after", 5, 0, sa(&a1), sizeof a1);
  n = recv(recv_fd, buf, sizeof buf, 0);
  CHECK(n == 5 && memcmp(buf, "after", 5) == 0, "child closing its copy does not unregister the parent's endpoint");

  puts("stale rows");
  {
    struct sockaddr_bp a3 = addr(10, 3);
    int x = socket(AF_BP, SOCK_DGRAM, 0);
    bind(x, sa(&a3), sizeof a3);
    int dn = open("/dev/null", O_WRONLY);
    dup2(dn, x); /* replaces the BP fd without going through close() */
    errno = 0;
    ssize_t w = write(x, "y", 1);
    int e = errno;
    errno = e;
    CHECK(w == 1, "after dup2 over a BP fd, write() reaches the new file, not the shim");
    close(x);

    int y = socket(AF_BP, SOCK_DGRAM, 0);
    bind(y, sa(&a3), sizeof a3);
    syscall(SYS_close_range, y, y, 0); /* closes it behind the shim's back */
    int z = open("/dev/null", O_WRONLY); /* the kernel hands the number out again */
    errno = 0;
    w = write(z, "y", 1);
    CHECK(z == y && w == 1, "a file reusing an unclosed BP fd number is not hijacked");
    close(z);
    close(dn);
  }

  puts("close / fd reuse");
  int old = send_fd;
  CHECK(close(send_fd) == 0, "close BP fd");
  CHECK(send(old, "x", 1, 0) == -1 && errno == EBADF, "closed fd is really closed (EBADF)");
  udp_passthrough("a reused fd number behaves as a normal socket");
  CHECK(close(recv_fd) == 0, "close receiver");

  printf("%s (%d failure%s)\n", failures ? "FAILED" : "ALL PASSED", failures, failures == 1 ? "" : "s");
  return failures ? 1 : 0;
}
