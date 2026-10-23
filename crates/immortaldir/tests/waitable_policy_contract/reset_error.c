/* Fail the owner's correction, and prove the broker was never created. */
#define _GNU_SOURCE
#include <dlfcn.h>
#include <errno.h>
#include <signal.h>
#include <stdio.h>
#include <unistd.h>
static pid_t owner;
__attribute__((constructor)) static void init(void) { owner = getpid(); }
int sigaction(int sig, const struct sigaction *action, struct sigaction *previous) {
    int (*real)(int, const struct sigaction *, struct sigaction *) = dlsym(RTLD_NEXT, "sigaction");
    if (getpid() == owner && sig == SIGCHLD && action && action->sa_handler == SIG_DFL) {
        errno = EPERM;
        return -1;
    }
    return real(sig, action, previous);
}
pid_t fork(void) {
    pid_t (*real)(void) = dlsym(RTLD_NEXT, "fork");
    if (getpid() == owner) fprintf(stderr, "UNEXPECTED FORK after reset failure\n");
    return real();
}
