package immortal

import (
	"os"
	"path/filepath"
	"syscall"
)

// MakeFifo creates a fifo file
func MakeFifo(path string) error {
	err := os.MkdirAll(filepath.Dir(path), os.ModePerm)
	if err != nil {
		return err
	}
	err = syscall.Mknod(path, syscall.S_IFIFO|0666, 0)
	// ignore "file exists" errors and assume the FIFO was pre-made
	if err != nil && !os.IsExist(err) {
		return err
	}
	return nil
}

// OpenFifo open fifo and returns its file descriptor
func OpenFifo(path string) (*os.File, error) {
	f, err := os.OpenFile(path, os.O_RDWR, os.ModeNamedPipe)
	if err != nil {
		return nil, err
	}
	return f, nil
}
