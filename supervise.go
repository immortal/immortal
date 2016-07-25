package immortal

import (
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

func (self *Daemon) Supervise() {
	self.Run(self.ctrl.state)

	for {
		select {
		case <-self.ctrl.quit:
			return
		case state := <-self.ctrl.state:
			if state != nil {
				if state.Error() == "EXIT" {
					log.Printf("PID: %d Exited", self.process.Pid)
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
					if pid > 1 && pid != self.process.Pid && self.isRunning(pid) {
						// set pid to new pid in file
						self.process.Pid = pid
						log.Printf("Watching pid %d on file: %s", self.process.Pid, self.run.FollowPid)
						go self.watchPid(pid, self.ctrl.state)
					} else {
						// if cmd exits or process is kill
						self.Run(self.ctrl.state)
					}
				}
			} else {
				self.Run(self.ctrl.state)
			}
		case fifo := <-self.ctrl.fifo:
			if fifo.err != nil {
				log.Printf("control error: %s", fifo.err)
			}
			self.handleSignals(fifo.msg, self.ctrl.state)
		}
	}
}
