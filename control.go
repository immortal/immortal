package immortal

import (
	"bufio"
	"io"
	"os"
	"strings"
)

type Control struct {
	fifo         chan Return
	fifo_control *os.File
	fifo_ok      *os.File
	quit         chan struct{}
	state        chan error
}

type Return struct {
	err error
	msg string
}

func (self *Sup) ReadFifoControl(fifo *os.File, ch chan<- Return) {
	r := bufio.NewReader(fifo)

	buf := make([]byte, 0, 8)

	go func() {
		defer fifo.Close()
		for {
			n, err := r.Read(buf[:cap(buf)])
			if n == 0 {
				if err == nil {
					continue
				}
				if err == io.EOF {
					continue
				}
				ch <- Return{err: err, msg: ""}
			}
			buf = buf[:n]
			ch <- Return{
				err: nil,
				msg: strings.ToLower(strings.TrimSpace(string(buf))),
			}
		}
	}()
}
