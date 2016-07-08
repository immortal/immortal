package immortal

import (
	"fmt"
	"time"
)

func (self *Daemon) Supervice() {
	go self.Run()
	for {
		select {
		case m := <-self.monitor:
			Log(Yellow(fmt.Sprintf("monitor: %v", m)))
		case err := <-self.status:
			if err != nil {
				Log(Red(err.Error()))
			}
			time.Sleep(1 * time.Second)
			go self.Run()
		}
	}
}
