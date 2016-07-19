// +build linux

package immortal

import (
	"fmt"
	"os"
	"time"
)

func (self *Daemon) watchPid(ch chan<- error) {
	for {
		if _, err := os.Stat(fmt.Sprintf("/proc/%d", self.pid)); os.IsNotExist(err) {
			ch <- fmt.Errorf("EXIT")
		}
		time.Sleep(time.Second)
	}
}
