package immortal

import (
	"bufio"
	"io/ioutil"
	"os"
	"strconv"
	"strings"
	"syscall"
)

type Supervisor interface {
	HandleSignals(signal string, d *Daemon)
	Info(ch <-chan os.Signal, d *Daemon)
	IsRunning(pid int) bool
	ReadFifoControl(fifo *os.File, ch chan<- Return)
	ReadPidFile(pidfile string) (int, error)
	WatchPid(pid int, ch chan<- error)
}

type Sup struct {
	process *process
}

func (self *Sup) IsRunning(pid int) bool {
	process, _ := os.FindProcess(pid)
	if err := process.Signal(syscall.Signal(0)); err != nil {
		return false
	}
	return true
}

// ReadPidfile read pid from file if error returns pid 0
func (self *Sup) ReadPidFile(pidfile string) (int, error) {
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

func (self *Sup) ReadFifoControl(fifo *os.File, ch chan<- Return) {
	r := bufio.NewReader(fifo)

	go func() {
		defer fifo.Close()
		for {
			s, err := r.ReadString('\n')
			if err != nil {
				ch <- Return{err: err, msg: ""}
			} else {
				ch <- Return{
					err: nil,
					msg: strings.ToLower(strings.TrimSpace(s)),
				}
			}
		}
	}()
}
