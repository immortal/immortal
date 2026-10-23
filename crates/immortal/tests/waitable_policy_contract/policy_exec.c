/* Install the disposition which POSIX exec preserves, then run the real CLI. */
#include <signal.h>
#include <unistd.h>
#include <stdio.h>
int main(int argc, char **argv) {
    struct sigaction action = {0};
    action.sa_handler = SIG_IGN;
    sigemptyset(&action.sa_mask);
    if (argc < 2 || sigaction(SIGCHLD, &action, NULL) != 0) return 70;
    execv(argv[1], argv + 1);
    perror("execv");
    return 70;
}
