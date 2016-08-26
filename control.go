package immortal

import (
	"fmt"
	"os"
	"sync/atomic"
	"time"
)

type controlPid struct {
	ch chan int
}

type controlOnce struct{}

type controlSignal struct {
	signal os.Signal
}

type controlKill struct{}

func (d *Daemon) Pid() int {
	ch := make(chan int, 1)
	d.ctrl <- controlPid{ch}
	return <-ch
}

func (d *Daemon) control(p *process) {
	for {
		select {
		case err := <-p.errch:
			fmt.Printf("d.process.sTime = %+v\n", time.Since(p.sTime))
			println(p.eTime.Sub(p.sTime))
			// lock_once defaults to 0, 1 to run only once/down (don't restart)
			atomic.StoreUint32(&d.lock, d.lock_once)
			fmt.Printf("err = %+v\n", err)
			d.done <- err
			return
		case ctrl := <-d.ctrl:
			switch c := ctrl.(type) {
			case controlPid:
				c.ch <- p.Pid()
				println(d.lock, d.lock_once)
			case controlOnce:
				d.lock_once = 1
			case controlSignal:
				p.Signal(c.signal)
			case controlKill:
				p.Kill()
			}
		}
	}
}
