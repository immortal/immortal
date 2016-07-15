package immortal

import (
	"time"
)

func (self *Daemon) Log(msg string) {
	self.logger.SetPrefix(time.Now().UTC().Format(time.RFC3339Nano) + " ")
	self.logger.Print(msg)
}
