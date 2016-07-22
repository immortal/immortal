package immortal

import (
	"bufio"
	"fmt"
	"io"
	"os"
	"strings"
	"syscall"
)

func (self *Daemon) makeFIFO(path string) (f *os.File, err error) {
	err = syscall.Mknod(path, syscall.S_IFIFO|0666, 0)
	// ignore "file exists" errors and assume the FIFO was pre-made
	if err != nil && !os.IsExist(err) {
		return
	}

	f, err = os.OpenFile(path, os.O_RDWR, os.ModeNamedPipe)
	if err != nil {
		return
	}
	return
}

func (self *Daemon) readFIFO(ctrl_fifo *os.File, ch chan<- Return) {
	r := bufio.NewReader(ctrl_fifo)

	buf := make([]byte, 0, 8)

	go func() {
		defer ctrl_fifo.Close()
		for {
			n, err := r.Read(buf[:cap(buf)])
			buf = buf[:n]
			if n == 0 {
				if err == nil {
					continue
				}
				if err == io.EOF {
					continue
				}
				self.ctrl.fifo <- Return{err: err, msg: ""}
			}
			self.ctrl.fifo <- Return{err: nil, msg: strings.TrimSpace(string(buf))}
		}
	}()
}
