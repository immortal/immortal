package immortal

import (
	"fmt"
	"os"
	"os/exec"
	"strconv"
	"syscall"
	"time"
)

const (
	logo = "2B55"
)

func init() {
	if os.Getppid() > 1 {
		cmd := exec.Command(os.Args[0])
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

func Log(s string) {
	t := time.Now().UTC().Format(time.RFC3339Nano)
	fmt.Printf("%s %s\n", t, s)
}

// Icon Unicode Hex to string
func Icon(h string) rune {
	i, e := strconv.ParseInt(h, 16, 32)
	if e != nil {
		return 0
	}
	return rune(i)
}
