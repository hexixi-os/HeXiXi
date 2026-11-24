#define _GNU_SOURCE
#include <errno.h>
#include <pthread.h>
#include <signal.h>
#include <stdatomic.h>
#include <stdint.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>
#include <sched.h>

enum { WORKERS = 8, ROUNDS = 16, ITERATIONS = 256 };
enum { FUTEX_WAIT_PRIVATE = 128, FUTEX_WAKE_PRIVATE = 129 };
static pthread_mutex_t lock = PTHREAD_MUTEX_INITIALIZER;
static pthread_cond_t ready_cv = PTHREAD_COND_INITIALIZER;
static pthread_cond_t start_cv = PTHREAD_COND_INITIALIZER;
static __thread int tls;
static _Atomic int futex_word;
static _Atomic int waiting;
static _Atomic int signal_tid;
static _Atomic int delivered_tid;
static int count, ready, started;
static pid_t tids[WORKERS];
static int output_fd;
static int handler_fd;
static volatile sig_atomic_t forked_in_handler;

static void check(int condition, const char *message)
{
    if (!condition) {
        fprintf(stderr, "pthread ABI: %s\n", message);
        exit(1);
    }
}

static void *worker(void *argument)
{
    const intptr_t index = (intptr_t)argument;
    tls = 100 + index;
    check(pthread_mutex_lock(&lock) == 0, "lock ready");
    tids[index] = syscall(SYS_gettid);
    ++ready;
    check(pthread_cond_signal(&ready_cv) == 0, "signal ready");
    while (!started)
        check(pthread_cond_wait(&start_cv, &lock) == 0, "wait start");
    check(pthread_mutex_unlock(&lock) == 0, "unlock start");

    for (int n = 0; n < ITERATIONS; ++n) {
        check(pthread_mutex_lock(&lock) == 0, "lock counter");
        ++count;
        if (n % 64 == 0)
            sched_yield();
        check(pthread_mutex_unlock(&lock) == 0, "unlock counter");
    }
    check(tls == 100 + index && getpid() != tids[index], "thread-local storage and TID");
    check(write(output_fd, "x", 1) == 1, "shared file descriptor");
    return (void *)(index + 1);
}

static void *futex_waiter(void *unused)
{
    (void)unused;
    atomic_store(&waiting, 1);
    long rc = syscall(SYS_futex, &futex_word, FUTEX_WAIT_PRIVATE, 0, 0, 0, 0);
    check(rc == 0 && atomic_load(&futex_word) == 0, "wake without changing futex word");
    return NULL;
}

static void on_signal(int signo)
{
    if (signo == SIGUSR1)
        atomic_store(&delivered_tid, syscall(SYS_gettid));
}

static void *signal_waiter(void *unused)
{
    (void)unused;
    sigset_t signals;
    sigemptyset(&signals);
    sigaddset(&signals, SIGUSR1);
    check(pthread_sigmask(SIG_BLOCK, &signals, NULL) == 0, "block targeted signal");
    atomic_store(&signal_tid, syscall(SYS_gettid));
    long rc = syscall(SYS_futex, &futex_word, FUTEX_WAIT_PRIVATE, 0, 0, 0, 0);
    check(rc == 0 && atomic_load(&delivered_tid) == 0, "masked signal stays pending");
    check(pthread_sigmask(SIG_UNBLOCK, &signals, NULL) == 0, "unblock targeted signal");
    check(atomic_load(&delivered_tid) == atomic_load(&signal_tid), "signal delivered to target");
    return NULL;
}

static void *group_exit(void *unused)
{
    (void)unused;
    syscall(SYS_exit_group, 37);
    abort();
}

static void *blocked_writer(void *arg)
{
    static char buffer[32768];
    atomic_store(&waiting, 1);
    check(write((int)(intptr_t)arg, buffer, sizeof(buffer)) >= 0, "blocked write");
    return NULL;
}

static void loop_in_handler(int signo)
{
    (void)signo;
    if (write(handler_fd, "s", 1) != 1)
        _exit(97);
    for (;;)
        atomic_signal_fence(memory_order_seq_cst);
}

static void fork_in_handler(int signo)
{
    (void)signo;
    forked_in_handler = fork();
}

int main(void)
{
    puts("pthread: create/join + TLS");
    pthread_t thread[WORKERS];
    int descriptors[2];
    check(pipe(descriptors) == 0, "pipe");
    output_fd = descriptors[1];
    tls = 99;
    for (int round = 0; round < ROUNDS; ++round) {
        count = ready = started = 0;
        for (intptr_t i = 0; i < WORKERS; ++i)
            check(pthread_create(&thread[i], NULL, worker, (void *)i) == 0, "create");
        check(pthread_mutex_lock(&lock) == 0, "lock start");
        while (ready != WORKERS)
            check(pthread_cond_wait(&ready_cv, &lock) == 0, "wait ready");
        for (int i = 0; i < WORKERS; ++i)
            for (int j = 0; j < i; ++j)
                check(tids[i] > 0 && tids[i] != tids[j], "unique thread IDs");
        started = 1;
        check(pthread_cond_broadcast(&start_cv) == 0, "broadcast start");
        check(pthread_mutex_unlock(&lock) == 0, "unlock start");
        for (intptr_t i = 0; i < WORKERS; ++i) {
            void *result;
            check(pthread_join(thread[i], &result) == 0 && (intptr_t)result == i + 1, "join");
        }
        check(count == WORKERS * ITERATIONS && tls == 99, "mutex and TLS stress");
        for (int i = 0; i < WORKERS; ++i) {
            char byte;
            check(read(descriptors[0], &byte, 1) == 1 && byte == 'x', "pipe data");
        }
    }
    puts("pthread: mutex + condition variable stress passed");
    check(pthread_create(&thread[0], NULL, futex_waiter, NULL) == 0, "create futex waiter");
    while (!atomic_load(&waiting))
        sched_yield();
    sched_yield();
    check(syscall(SYS_futex, &futex_word, FUTEX_WAKE_PRIVATE, 1, 0, 0, 0) == 1, "wake one");
    check(pthread_join(thread[0], NULL) == 0, "join futex waiter");
    check(syscall(SYS_futex, &futex_word, FUTEX_WAKE_PRIVATE, 1, 0, 0, 0) == 0, "wake nobody");
    check(syscall(SYS_futex, &futex_word, FUTEX_WAIT_PRIVATE, 1, 0, 0, 0) == -1 &&
          errno == EAGAIN, "futex value mismatch");
    struct timespec timeout = {0, 2000000};
    check(syscall(SYS_futex, &futex_word, FUTEX_WAIT_PRIVATE, 0, &timeout, 0, 0) == -1 &&
          errno == ETIMEDOUT, "futex timeout");
    struct sigaction action = {.sa_handler = on_signal};
    sigemptyset(&action.sa_mask);
    check(sigaction(SIGUSR1, &action, NULL) == 0, "set signal handler");
    check(pthread_create(&thread[0], NULL, signal_waiter, NULL) == 0, "create signal target");
    while (!atomic_load(&signal_tid))
        sched_yield();
    check(pthread_kill(thread[0], SIGUSR1) == 0 && atomic_load(&delivered_tid) == 0, "signal target");
    check(syscall(SYS_futex, &futex_word, FUTEX_WAKE_PRIVATE, 1, 0, 0, 0) == 1, "wake signal target");
    check(pthread_join(thread[0], NULL) == 0, "join signal target");
    pid_t child = fork();
    check(child >= 0, "fork exit_group test");
    if (child == 0) {
        check(pthread_create(&thread[0], NULL, group_exit, NULL) == 0, "create exit_group caller");
        pthread_join(thread[0], NULL);
        _exit(1);
    }
    int status;
    check(waitpid(child, &status, 0) == child && WIFEXITED(status) && WEXITSTATUS(status) == 37,
          "exit_group from worker");
    int clean[2];
    check(pipe2(clean, O_CLOEXEC) == 0, "CLOEXEC pipe");
    const int keep = fcntl(clean[0], F_DUPFD, 20);
    check(keep >= 20 && fcntl(keep, F_GETFD) == 0, "plain dup clears CLOEXEC");
    check(fcntl(clean[0], F_GETFD) == FD_CLOEXEC, "pipe fd reports CLOEXEC");
    child = fork();
    check(child >= 0, "fork descriptor test");
    if (child == 0) {
        char closed[16], inherited[16];
        snprintf(closed, sizeof(closed), "%d", clean[0]);
        snprintf(inherited, sizeof(inherited), "%d", keep);
        execl("./busybox", "busybox", "sh", "-c",
              ": <&$2 && ! ( : <&$1 ) 2>/dev/null",
              "fd-probe", closed, inherited, (char *)0);
        _exit(99);
    }
    check(waitpid(child, &status, 0) == child && WIFEXITED(status) && WEXITSTATUS(status) == 0,
          "exec closes only CLOEXEC descriptors");
    check(pipe(descriptors) == 0, "handler pipe");
    handler_fd = descriptors[1];
    child = fork();
    check(child >= 0, "fork signal termination test");
    if (child == 0) {
        action.sa_handler = loop_in_handler;
        check(sigaction(SIGUSR1, &action, NULL) == 0, "register looping handler");
        raise(SIGUSR1);
        _exit(98);
    }
    char byte;
    check(read(descriptors[0], &byte, 1) == 1 && byte == 's', "handler started");
    check(kill(child, SIGKILL) == 0 && waitpid(child, &status, 0) == child &&
          WIFSIGNALED(status) && WTERMSIG(status) == SIGKILL, "SIGKILL during handler");
    action.sa_handler = fork_in_handler;
    check(sigaction(SIGUSR1, &action, NULL) == 0 && raise(SIGUSR1) == 0, "fork from signal handler");
    if (forked_in_handler == 0)
        _exit(41);
    check(forked_in_handler > 0 && waitpid(forked_in_handler, &status, 0) == forked_in_handler &&
          WIFEXITED(status) && WEXITSTATUS(status) == 41, "child returns from signal handler");
    check(pipe(descriptors) == 0, "blocked pipe");
    atomic_store(&waiting, 0);
    child = fork();
    check(child >= 0, "fork blocked writer");
    if (child == 0) {
        close(descriptors[0]);
        check(pthread_create(&thread[0], NULL, blocked_writer,
                             (void *)(intptr_t)descriptors[1]) == 0, "create blocked writer");
        while (!atomic_load(&waiting))
            sched_yield();
        _exit(42);
    }
    close(descriptors[1]);
    check(waitpid(child, &status, 0) == child && WIFEXITED(status) && WEXITSTATUS(status) == 42,
          "exit_group with blocked writer");
    char block[4096];
    ssize_t drained;
    do {
        drained = read(descriptors[0], block, sizeof(block));
    } while (drained > 0);
    check(drained == 0, "blocked worker releases pipe writer at exit");
    puts("pthread-abi-test-ok");
    return 0;
}
