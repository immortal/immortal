package immortal

import (
	"bufio"
	"io"
	"io/ioutil"
	"log"
	"os"
	"os/exec"
	"os/signal"
	"strconv"
	"strings"
	"syscall"
	"time"
)

type Supervisor interface {
	HandleSignals(signal string, d *Daemon)
	Info(ch <-chan os.Signal, d *Daemon)
	IsRunning(pid int) bool
	ReadFifoControl(fifo *os.File, ch chan<- Return)
	ReadPidFile(pidfile string) (int, error)
	WatchPid(pid int, ch chan<- error)
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

func (self *Sup) ReadFifoControl(fifo *os.File, ch chan<- Return) {
	r := bufio.NewReader(fifo)

	buf := make([]byte, 0, 8)

	go func() {
		defer fifo.Close()
		for {
			n, err := r.Read(buf[:cap(buf)])
			if n == 0 {
				if err == nil {
					continue
				}
				if err == io.EOF {
					continue
				}
				ch <- Return{err: err, msg: ""}
			}
			buf = buf[:n]
			ch <- Return{
				err: nil,
				msg: strings.ToLower(strings.TrimSpace(string(buf))),
			}
		}
	}()
}

func Supervise(s Supervisor, d *Daemon) {
	// listen on control for signals
	if d.ctrl {
		s.ReadFifoControl(d.Control.fifo_control, d.Control.fifo)
	}

	// info channel
	info := make(chan os.Signal)
	signal.Notify(info, syscall.SIGQUIT)
	go s.Info(info, d)

	// run loop
	run := make(chan struct{}, 1)
	for {
		select {
		case <-d.Control.quit:
			return
		case <-run:
			time.Sleep(time.Second)
			d.Run(NewProcess(d.Config))
		default:
			select {
			case state := <-d.Control.done:
				if state != nil {
					if exitError, ok := state.(*exec.ExitError); ok {
						log.Printf("PID %d terminated, %s [%v user  %v sys  %s up]\n",
							exitError.Pid(),
							exitError,
							exitError.UserTime(),
							exitError.SystemTime(),
							"d.process.Uptime()")
					} else if state.Error() == "EXIT" {
						log.Printf("PID: %d Exited", d.process.Pid())
					} else {
						log.Print(state)
					}
				}

				// follow the new pid and stop running the command
				// unless the new pid dies
				if d.Config.Pid.Follow != "" {
					pid, err := s.ReadPidFile(d.Config.Pid.Follow)
					if err != nil {
						log.Printf("Cannot read pidfile:%s,  %s", d.Config.Pid.Follow, err)
						run <- struct{}{}
					} else {
						// check if pid in file is valid
						if pid > 1 && pid != d.process.Pid() && s.IsRunning(pid) {
							// set pid to new pid in file
							// -------> fix this d.Process.Pid = pid
							log.Printf("Watching pid %d on file: %s", d.process.Pid(), d.Config.Pid.Follow)
							go s.WatchPid(pid, d.Control.done)
						} else {
							// if cmd exits or process is kill
							run <- struct{}{}
						}
					}
				} else {
					run <- struct{}{}
				}
			case fifo := <-d.Control.fifo:
				if fifo.err != nil {
					log.Printf("control error: %s", fifo.err)
				}
				go s.HandleSignals(fifo.msg, d)
			}
		}
	}
}
