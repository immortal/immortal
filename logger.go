package immortal

import (
//	"time"
)

func (self *Daemon) Log(msg string) {
	self.logger.Print(msg)
}
