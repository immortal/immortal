// +build linux

package immortal

import (
	"fmt"
	"os"
	"time"
)

func (self *Daemon) watchPid(ch chan<- error) {
	initialStat, err := os.Stat(self.run.FollowPid)
	if err != nil {
		ch <- err
		return
	}

	for {
		stat, err := os.Stat(self.run.FollowPid)
		if err != nil {
			ch <- err
			return
		}

		if stat.Size() != initialStat.Size() || stat.ModTime() != initialStat.ModTime() {
			ch <- fmt.Errorf("EXIT")
			return
		}

		time.Sleep(time.Second)
	}
}
