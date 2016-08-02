// +build linux

package immortal

import (
	"fmt"
	"os"
	"time"
)

func (self *Sup) WatchPid(pid int, ch chan<- error) {
	if !self.IsRunning(pid) {
		ch <- fmt.Errorf("PID NOT FOUND")
		return
	}

	for {
		if _, err := os.Stat(fmt.Sprintf("/proc/%d", pid)); err != nil {
			ch <- fmt.Errorf("EXIT")
			return
		}
		time.Sleep(time.Second)
	}
}
