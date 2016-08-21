package immortal

import "os"

type Control struct {
	fifo         chan Return
	fifo_control *os.File
	fifo_ok      *os.File
	quit         chan struct{}
	done         chan error
}

type Return struct {
	err error
	msg string
}
