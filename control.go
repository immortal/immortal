package immortal

import ()

type Controller interface {
	Send(e error)
}

type Control struct {
	fifo  chan Return
	quit  chan struct{}
	state chan error
}

type Return struct {
	err error
	msg string
}

func (self *Control) Send(e error) {
	self.state <- e
}
