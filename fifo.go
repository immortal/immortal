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
	// create status pipe
	status_fifo := fmt.Sprintf("%s/status", self.sdir)
	syscall.Mknod(status_fifo, syscall.S_IFIFO|0666, 0)

	file, err := os.OpenFile(self.sdir+"/status", os.O_RDWR, os.ModeNamedPipe)
	if err != nil {
		return err
	}

	r := bufio.NewReader(file)
	buf := make([]byte, 0, 8)

	go func() {
		defer file.Close()
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
