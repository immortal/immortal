/* GNU/Linux-only signal authority oracle. Never deliver an unsafe test kill. */
#define _GNU_SOURCE
#include <errno.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

static int modeled;
int kill(pid_t pid, int sig) {
    const char *mode = getenv("IMMORTAL_GUARD_MODE");
    if (pid < -1 && sig == 0 && mode && strcmp(mode, "reused") == 0 && !modeled++) {
        return 0; /* Model a new unrelated group with the released ID. */
    }
    if (sig == SIGKILL && (pid > 0 || pid < -1)) {
        siginfo_t info = {0};
        pid_t owner = pid > 0 ? pid : -pid;
        if (waitid(P_PID, owner, &info, WEXITED | WNOHANG | WNOWAIT) < 0) {
            fprintf(stderr, "UNSAFE %s signal pid=%d: no owned unreaped leader; suppressed\n", pid > 0 ? "stale PID" : "unpinned group", pid);
            errno = ESRCH;
            return -1;
        }
    }
    return syscall(SYS_kill, pid, sig);
}
