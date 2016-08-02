package immortal

type Control struct {
	fifo  chan Return
	quit  chan struct{}
	state chan error
}

type Return struct {
	err error
	msg string
}
