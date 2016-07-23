package immortal

import (
	"fmt"
	"io/ioutil"
	"os"
	"os/exec"
	"strconv"
	"strings"
	"syscall"
)

const (
	logo = "2B55"
)

func Logo() rune {
	return Icon(logo)
}

// Icon Unicode Hex to string
func Icon(h string) rune {
	i, e := strconv.ParseInt(h, 16, 32)
	if e != nil {
		return 0
	}
	return rune(i)
}

func Lock(f string) error {
	file, err := os.Create(f)
	if err != nil {
		return err
	}
	return syscall.Flock(int(file.Fd()), syscall.LOCK_EX+syscall.LOCK_NB)
}

func MakeFIFO(path string) (f *os.File, err error) {
	err = syscall.Mknod(path, syscall.S_IFIFO|0666, 0)
	// ignore "file exists" errors and assume the FIFO was pre-made
	if err != nil && !os.IsExist(err) {
		return
	}

	f, err = os.OpenFile(path, os.O_RDWR, os.ModeNamedPipe)
	if err != nil {
		return
	}
	return
}

func Fork() (int, error) {
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

// ReadPidfile read pid from file if error returns pid 0
func ReadPidfile(pidfile string) (int, error) {
	content, err := ioutil.ReadFile(pidfile)
	if err != nil {
		return 0, err
	}
	lines := strings.Split(string(content), "\n")
	pid, err := strconv.Atoi(lines[0])
	if err != nil {
		return 0, err
	}
	return pid, nil
}

func WritePid(file string, pid int) error {
	if err := ioutil.WriteFile(file, []byte(fmt.Sprintf("%d", pid)), 0644); err != nil {
		return err
	}
	return nil
}
