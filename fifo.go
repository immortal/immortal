package immortal

import (
	"bufio"
	"fmt"
	"io"
	"os"
	"strings"
	"syscall"
)

func (self *Daemon) FIFO() error {
	// create control pipe
	fifo := fmt.Sprintf("%s/control", self.sdir)
	syscall.Mknod(fifo, syscall.S_IFIFO|0666, 0)

	ctrl_fifo, err := os.OpenFile(self.sdir+"/control", os.O_RDWR, os.ModeNamedPipe)
	if err != nil {
		return err
	}

	// create status pipe
	fifo = fmt.Sprintf("%s/status", self.sdir)
	syscall.Mknod(fifo, syscall.S_IFIFO|0666, 0)

	status_fifo, err := os.OpenFile(self.sdir+"/status", os.O_RDWR, os.ModeNamedPipe)
	if err != nil {
		return err
	}
	self.ctrl.status = status_fifo

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
				self.ctrl.err <- err
			}
			self.ctrl.fifo <- strings.TrimSpace(string(buf))
		}
	}()

	return nil
}
