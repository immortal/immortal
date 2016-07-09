package immortal

import (
	"fmt"
	"io/ioutil"
	"strconv"
	"strings"
	"time"
)

func (self *Daemon) Supervice() {
	go self.Run()

	var monitor_pid bool
	for {
		select {
		case <-self.monitor:
			// give time to write
			time.Sleep(1 * time.Second)

			content, err := ioutil.ReadFile(self.Pidfile)
			if err != nil {
				Log(Yellow(fmt.Sprintf("monitor: %s", err.Error())))
			}

			lines := strings.Split(string(content), "\n")

			pid, err := strconv.Atoi(lines[0])
			if err != nil {
				Log(Red(fmt.Sprintf("Bad process id found in %s, %s", self.Pidfile, err.Error())))
			}

			Log(fmt.Sprintf("PID on file: %v Parent Pid: %d", pid, self.pid))

			// Monitor the new childs and stop restarting the old process
			if pid != self.pid {
				monitor_pid = true
			}
			go self.Monitor()
		case err := <-self.err:
			if err != nil {
				Log(Red(err.Error()))
			}
		case wait := <-self.status:
			if wait != nil {
				Log(Red(wait.Error()))
			}
			if !monitor_pid {
				time.Sleep(1 * time.Second)
				go self.Run()
			}
		}
	}
}
