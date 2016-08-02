package immortal

type Controller interface {
	SendState(e error)
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

func (self *Control) SendState(e error) {
	self.state <- e
}
