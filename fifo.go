package immortal

import (
	"os"
	"syscall"
)

type FIFOer interface {
	Make(path string) error
	Open(path string) (*os.File, error)
}

type FIFO struct{}

func (self *FIFO) Make(path string) (err error) {
	err = syscall.Mknod(path, syscall.S_IFIFO|0666, 0)
	// ignore "file exists" errors and assume the FIFO was pre-made
	if err != nil && !os.IsExist(err) {
		return
	}
	return
}

func (self *FIFO) Open(path string) (f *os.File, err error) {
	f, err = os.OpenFile(path, os.O_RDWR, os.ModeNamedPipe)
	if err != nil {
		return
	}
	return
}
