package immortal

import (
	"fmt"
	"io/ioutil"
	"strconv"
	"strings"
	"time"
)

func (self *Daemon) readPidfile() (int, error) {
	content, err := ioutil.ReadFile(self.run.Pidfile)
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
	go self.Run()

	for {
		select {
		case <-self.wPid:
			Log(Yellow("-----------"))
		case err := <-self.err:
			if err != nil {
				Log(Red(err.Error()))
			}
		case run := <-self.status:
			if run != nil {
				Log(Red(run.Error()))
			}
			time.Sleep(1 * time.Second)
			// follow the new pid and stop running the command
			// unless the new pid dies
			if self.run.Pidfile != "" {
				pid, err := self.readPidfile()
				if err != nil {
					Log(Red(fmt.Sprintf("Cannot read pidfile:%s,  %s", self.run.Pidfile, err.Error())))
				}
				// check if pid in file is valid
				if pid > 1 && pid != self.pid {
					// set pid to new pid in file
					self.pid = pid
					go self.watchPid()
				} else {
					go self.Run()
				}
			} else {
				go self.Run()
			}
		}
	}
}
