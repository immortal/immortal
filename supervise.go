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
	for {
		select {
		case m := <-self.monitor:
			content, err := ioutil.ReadFile(self.Pidfile)

			if err != nil {
				Log(Yellow(fmt.Sprintf("monitor: %s", m.Error())))
			}

			lines := strings.Split(string(content), "\n")
			pid, err := strconv.Atoi(lines[0])
			if err != nil {
				Log(Red(fmt.Sprintf("Bad process id found in %s, %s", self.Pidfile, err.Error())))
			}

			Log(Green(fmt.Sprintf("PID on file: %v Parent Pid: %d", pid, self.pid)))
			time.Sleep(1 * time.Second)
			go self.Monitor()
		case err := <-self.status:
			if err != nil {
				Log(Red(err.Error()))
			}
			time.Sleep(1 * time.Second)
			go self.Run()
		}
	}
}
