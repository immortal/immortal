package immortal

import "os"

type Control struct {
	done         chan error
	fifo         chan Return
	fifo_control *os.File
	fifo_ok      *os.File
	quit         chan struct{}
	running      chan struct{}
}

type Return struct {
	err error
	msg string
}
