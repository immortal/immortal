package immortal

import (
	"os"
	"os/exec"
	"syscall"
)

// Fork crete a new process
func Fork() (int, error) {
	args := os.Args[1:]
	cmd := exec.Command(os.Args[0], args...)
	cmd.Env = os.Environ()
	cmd.Stdin = nil
	cmd.Stdout = nil
	cmd.Stderr = nil
	cmd.ExtraFiles = nil
	cmd.SysProcAttr = &syscall.SysProcAttr{
		Setsid: true, // setsid is used to detach the process from the parent (normally a shell)
	}
	if err := cmd.Start(); err != nil {
		return 0, err
	}
	return cmd.Process.Pid, nil
}
