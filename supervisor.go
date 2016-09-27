package immortal

import (
	"io/ioutil"
	"os"
	"strconv"
	"strings"
	"syscall"
)

// Supervisor interface
type Supervisor interface {
	HandleSignals(signal string, d *Daemon)
	Info(ch <-chan os.Signal, d *Daemon)
	IsRunning(pid int) bool
	ReadPidFile(pidfile string) (int, error)
	WatchPid(pid int, ch chan<- error)
}

// Sup implements Supervisor
type Sup struct {
	daemon  *Daemon
	process *process
}

// IsRunning check if process is running
func (s *Sup) IsRunning(pid int) bool {
	process, _ := os.FindProcess(pid)
	if err := process.Signal(syscall.Signal(0)); err != nil {
		return false
	}
	return true
}

// ReadPidFile read pid from file if error returns pid 0
func (s *Sup) ReadPidFile(pidfile string) (int, error) {
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
