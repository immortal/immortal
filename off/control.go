package immortal

import (
	"bufio"
	"io"
	"strings"
)

func ControlXX() {
	r := bufio.NewReader(self.ctrl.control_fifo)

	buf := make([]byte, 0, 8)

	go func() {
		defer self.ctrl.control_fifo.Close()
		for {
			n, err := r.Read(buf[:cap(buf)])
			if n == 0 {
				if err == nil {
					continue
				}
				if err == io.EOF {
					continue
				}
				self.ctrl.fifo <- Return{err: err, msg: ""}
			}
			buf = buf[:n]
			self.ctrl.fifo <- Return{
				err: nil,
				msg: strings.ToLower(strings.TrimSpace(string(buf))),
			}
		}
	}()
}
