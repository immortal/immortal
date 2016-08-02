package immortal

import (
	"fmt"
	"io/ioutil"
	"log"
	"os"
	"strconv"
	"strings"
	"syscall"
	"time"
)

type Supervisor interface {
	IsRunning(pid int) bool
	ReadPidFile(pidfile string) (int, error)
	WatchPid(pid int, ch chan<- error)
	ReadFifoControl(fifo *os.File, ch chan<- Return)
}

type Sup struct{}

func (self *Sup) IsRunning(pid int) bool {
	process, _ := os.FindProcess(int(pid))
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

func Supervise(s Supervisor, d *Daemon) {
	if d.ctrl {
		s.ReadFifoControl(d.Control.fifo_control, d.Control.fifo)
	}
	for {
		select {
		case <-d.Control.quit:
			return
		case state := <-d.Control.state:
			if state != nil {
				if state.Error() == "EXIT" {
					log.Printf("PID: %d Exited", d.process.Pid)
				} else {
					log.Print(state.Error())
				}
			}

			// settle down, give time for writing the PID and avoid consuming CPU
			time.Sleep(time.Second)

			// follow the new pid and stop running the command
			// unless the new pid dies
			if d.Pid.Follow != "" {
				pid, err := s.ReadPidFile(d.Pid.Follow)
				if err != nil {
					log.Printf("Cannot read pidfile:%s,  %s", d.Pid.Follow, err.Error())
					d.Run()
				} else {
					// check if pid in file is valid
					if pid > 1 && pid != d.process.Pid && s.IsRunning(pid) {
						// set pid to new pid in file
						d.process.Pid = pid
						log.Printf("Watching pid %d on file: %s", d.process.Pid, d.Pid.Follow)
						go s.WatchPid(pid, d.Control.state)
					} else {
						// if cmd exits or process is kill
						d.Run()
					}
				}
			} else {
				d.Run()
			}
		case fifo := <-d.Control.fifo:
			if fifo.err != nil {
				log.Printf("control error: %s", fifo.err)
			}
			fmt.Fprintf(d.Control.fifo_ok, "pong: %s\n", fifo.msg)
			//			go self.handleSignals(fifo.msg, self.ctrl.state)
		}
	}
}
