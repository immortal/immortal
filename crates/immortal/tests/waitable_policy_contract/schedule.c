/* Force the real broker's normal exit between the final idle wait and KILL.
 * Also test rejection after deliberate policy/collection contract violations.
 * Unsafe deliveries are recorded and suppressed, never sent to a released ID.
 */
#define _GNU_SOURCE
#include <dlfcn.h>
#include <errno.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

static pid_t owner, broker;
static int injected;
static const char *mode;
__attribute__((constructor)) static void init(void) {
    owner = getpid();
    mode = getenv("IMMORTAL_POLICY_SCHEDULE");
}
static int is_mode(const char *value) { return mode && strcmp(mode, value) == 0; }

pid_t fork(void) {
    pid_t (*real_fork)(void) = dlsym(RTLD_NEXT, "fork");
    pid_t pid = real_fork();
    if (getpid() == owner && pid > 0) {
        broker = pid;
        if (is_mode("reset-error")) fprintf(stderr, "UNEXPECTED FORK after reset failure\n");
    }
    else if (pid == 0 && broker == 0) broker = getpid();
    return pid;
}

int sigaction(int sig, const struct sigaction *action, struct sigaction *previous) {
    int (*real_action)(int, const struct sigaction *, struct sigaction *) = dlsym(RTLD_NEXT, "sigaction");
    if (getpid() == owner && sig == SIGCHLD && action && action->sa_handler == SIG_DFL && is_mode("reset-error")) {
        errno = EPERM;
        return -1;
    }
    int result = real_action(sig, action, previous);
    if (result == 0 && getpid() == owner && sig == SIGCHLD && !action && previous
        && previous->sa_handler == SIG_IGN && injected == 1 && is_mode("changed")) {
        injected = 2;
        /* The broker was live at policy observation. Only now let it exit. */
        fprintf(stderr, "POLICY QUERY observed live auto-reaping owner\n");
        syscall(SYS_kill, broker, SIGCONT);
    }
    return result;
}

__attribute__((noreturn)) void exit(int code) {
    if (getpid() == broker && broker != owner) {
        fprintf(stderr, "broker paused before normal exit\n");
        syscall(SYS_kill, broker, SIGSTOP);
    }
    void (*real_exit)(int) = dlsym(RTLD_NEXT, "exit");
    real_exit(code);
    __builtin_unreachable();
}

pid_t waitpid(pid_t pid, int *status, int flags) {
    pid_t (*real_wait)(pid_t, int *, int) = dlsym(RTLD_NEXT, "waitpid");
    pid_t result = real_wait(pid, status, flags);
    if (getpid() == owner && pid == -1 && result == 0 && !injected && (is_mode("changed") || is_mode("stolen"))) {
        /* Keep the real idle result until the supervisor's five-second
         * deadline has certainly elapsed, without fabricating a wait event. */
        struct timespec delay = {5, 50000000};
        while (nanosleep(&delay, &delay) < 0 && errno == EINTR) {}
        {
            injected = 1;
            if (is_mode("changed")) {
                struct sigaction action = {0};
                action.sa_handler = SIG_IGN;
                sigemptyset(&action.sa_mask);
                sigaction(SIGCHLD, &action, NULL);
            }
            if (is_mode("stolen")) {
                syscall(SYS_kill, broker, SIGCONT);
                /* A competing collector removes ownership before cleanup. */
                int terminal;
                while (real_wait(broker, &terminal, 0) < 0 && errno == EINTR) {}
            }
            fprintf(stderr, "INJECTED %s after idle wait\n", mode);
        }
    }
    return result;
}

int kill(pid_t pid, int sig) {
    if (getpid() == owner && pid == broker && sig == SIGKILL) {
        fprintf(stderr, "DELIVERY at broker deadline\n");
        siginfo_t before = {0};
        if (waitid(P_PID, broker, &before, WEXITED | WNOHANG | WNOWAIT) < 0) {
            fprintf(stderr, "STALE PID KILL without child ownership; suppressed\n");
            errno = ESRCH;
            return -1;
        }
        syscall(SYS_kill, broker, SIGCONT);
        for (int n = 0; n < 2000; ++n) {
            siginfo_t info = {0};
            int result = waitid(P_PID, broker, &info, WEXITED | WNOHANG | WNOWAIT);
            if (result < 0 && errno == ECHILD) {
                fprintf(stderr, "STALE PID KILL auto-reaped or stolen; suppressed\n");
                errno = ESRCH;
                return -1;
            }
            if (result == 0 && info.si_pid == broker) {
                fprintf(stderr, "PINNED terminal broker at delivery\n");
                break;
            }
            usleep(1000);
        }
    }
    return syscall(SYS_kill, pid, sig);
}
