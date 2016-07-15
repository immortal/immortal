package immortal

import (
	"fmt"
	"io/ioutil"
	"strconv"
	"strings"
	"time"
)

// readPidfile read pid from file if error returns pid 0
func (self *Daemon) readPidfile() (int, error) {
	content, err := ioutil.ReadFile(self.run.FollowPid)
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

func (self *Daemon) Supervice() {
	self.Run(self.ctrl.state)

	for {
		select {
		case <-self.ctrl.quit:
			return
		case state := <-self.ctrl.state:
			if state != nil {
				self.Log(Yellow(state.Error()))

				if state.Error() == "EXIT" {
					self.Log(Yellow(fmt.Sprintf("PID: %d Exited", self.pid)))
				}
			}

			// settle down, give time for writing the PID and avoid consuming CPU
			time.Sleep(1 * time.Second)

			// follow the new pid and stop running the command
			// unless the new pid dies
			if self.run.FollowPid != "" {
				pid, err := self.readPidfile()
				if err != nil {
					self.Log(Red(fmt.Sprintf("Cannot read pidfile:%s,  %s", self.run.FollowPid, err.Error())))
				}
				// check if pid in file is valid
				if pid > 1 && pid != self.pid {
					// set pid to new pid in file
					self.pid = pid
					self.Log(Yellow(fmt.Sprintf("Starting to watch pid %d in file: %s", self.pid, self.run.FollowPid)))
					//	go self.watchPid()
				} else {
					//go self.Run()
				}
			} else {
				//go self.Run()
			}
		case fifo := <-self.ctrl.fifo:
			self.Log(Yellow(fmt.Sprintf("fifo: %s", fifo)))
			fmt.Fprintf(self.ctrl.status, "pong: %s\n", fifo)
		}
	}
}
