package immortal

import (
	"fmt"
	"os"
	"os/exec"
	"syscall"
)

func Fork() {
	if os.Getppid() > 1 {
		args := os.Args[1:]
		cmd := exec.Command(os.Args[0], args...)
		cmd.Start()
		fmt.Printf("%c   %d", Icon(logo), cmd.Process.Pid)
		os.Exit(0)
	}

	os.Chdir("/")
	_ = syscall.Umask(0)
	_, err := syscall.Setsid()
	if err != nil {
		fmt.Printf("Error: syscall.Setsid errno: %d", err)
		os.Exit(1)
	}
}
