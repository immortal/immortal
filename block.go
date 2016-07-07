package immortal

import (
	"os"
	"os/signal"
)

func (self *Daemon) Block() {
	c := make(chan os.Signal)
	signal.Notify(c, os.Interrupt, os.Kill)
	<-c
	os.Exit(0)
}
