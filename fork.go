package immortal

import (
	"os"
	"os/exec"
	"syscall"
)

//		fmt.Printf("%c   %d", Icon(logo), cmd.Process.Pid)
// Log(fmt.Sprintf("%c   %d", Icon(logo), pid))
// _ = syscall.Umask(0)
// pid, err := syscall.Setsid()

func (self *Daemon) Fork() error {
	args := os.Args[1:]
	cmd := exec.Command(os.Args[0], args...)
	cmd.Env = append(cmd.Env, os.Environ()...)
	cmd.Dir = "/"
	cmd.Stdin = nil
	cmd.Stdout = nil
	cmd.Stderr = nil
	cmd.ExtraFiles = nil
	cmd.SysProcAttr = &syscall.SysProcAttr{Setsid: false, Setpgid: true}
	return cmd.Start()
}
