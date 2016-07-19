package immortal

import (
	"os"
	"os/exec"
	"syscall"
)

func (self *Daemon) Fork() (int, error) {
	if os.Getppid() > 1 {
		args := os.Args[1:]
		cmd := exec.Command(os.Args[0], args...)
		cmd.Env = append(cmd.Env, os.Environ()...)
		cmd.Stdin = nil
		cmd.Stdout = nil
		cmd.Stderr = nil
		cmd.ExtraFiles = nil
		cmd.SysProcAttr = &syscall.SysProcAttr{
			Setpgid: true,
			Pgid:    0,
		}
		if err := cmd.Start(); err != nil {
			return 0, err
		}
		return cmd.Process.Pid, nil
	}
	return 0, nil
}
