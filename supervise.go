package immortal

import (
	"fmt"
	"time"
)

func (self *Daemon) Supervice() {
	go self.Run()
	for {
		select {
		case pid := <-self.pid:
			Log(Yellow(fmt.Sprintf("pid: %d", pid)))
		case err := <-self.status:
			if err != nil {
				Log(Red(err.Error()))
			}
			time.Sleep(1 * time.Second)
			go self.Run()
		}
	}
}
