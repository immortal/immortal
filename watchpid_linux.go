// +build linux

package immortal

import (
	"fmt"
	"os"
	"time"
)

// WatchPid check pid changes
func (d *Daemon) WatchPid(pid int, ch chan<- error) {
	if !d.IsRunning(pid) {
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
