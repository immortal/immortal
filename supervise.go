package immortal

import (
	"fmt"
	"log"
	"os"
	"syscall"
	"time"
)

func (self *Daemon) isRunning(pid int) bool {
	process, _ := os.FindProcess(int(pid))
	if err := process.Signal(syscall.Signal(0)); err != nil {
		return false
	}
	return true
}

func (self *Daemon) Supervice() {
	self.Run(self.ctrl.state)

	for {
		select {
		case <-self.ctrl.quit:
			return
		case state := <-self.ctrl.state:
			if state != nil {
				if state.Error() == "EXIT" {
					log.Printf("PID: %d Exited", self.pid)
				} else {
					log.Print(state.Error())
				}
			}

			// settle down, give time for writing the PID and avoid consuming CPU
			time.Sleep(time.Second)

			// follow the new pid and stop running the command
			// unless the new pid dies
			if self.run.FollowPid != "" {
				pid, err := ReadPidfile(self.run.FollowPid)
				if err != nil {
					log.Printf("Cannot read pidfile:%s,  %s", self.run.FollowPid, err.Error())
					self.Run(self.ctrl.state)
				} else {
					// check if pid in file is valid
					if pid > 1 && pid != self.pid && self.isRunning(pid) {
						// set pid to new pid in file
						self.pid = pid
						log.Printf("Watching pid %d on file: %s", self.pid, self.run.FollowPid)
						go self.watchPid(pid, self.ctrl.state)
					} else {
						self.Run(self.ctrl.state)
					}
				}
			} else {
				self.Run(self.ctrl.state)
			}
		case fifo := <-self.ctrl.fifo:
			log.Printf("fifo: %s", fifo)
			fmt.Fprintf(self.ctrl.status_fifo, "pong: %s\n", fifo)
		}
	}
}
