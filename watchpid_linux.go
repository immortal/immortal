package immortal

import (
	"fmt"
	"os"
	"syscall"
	"time"
)

func (self *Daemon) watchPid(ch chan<- error) {
	for {
		process, err := os.FindProcess(self.pid)
		if err != nil {
			ch <- err
			return
		}

		err = process.Signal(syscall.Signal(0))
		if err != nil {
			ch <- fmt.Errorf("EXIT")
		}
		time.Sleep(time.Second)
	}
}
