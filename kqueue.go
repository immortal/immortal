package immortal

import (
	"fmt"
	"log"
	"os"
	"strconv"
	"syscall"
)

func (self *Daemon) monitorPid() {
	kq, err := syscall.Kqueue()
	if err != nil {
		self.monitor <- err
		return
	}

	ev1 := syscall.Kevent_t{
		Ident:  uint64(process.Pid),
		Filter: syscall.EVFILT_PROC,
		Flags:  syscall.EV_ADD,
		Fflags: syscall.NOTE_EXIT,
		Data:   0,
		Udata:  nil,
	}

	for {
		events := make([]syscall.Kevent_t, 1)
		n, err := syscall.Kevent(kq, []syscall.Kevent_t{ev1}, events, nil)
		if err != nil {
			self.monitor <- err
			return
		}
		for i := 0; i < n; i++ {
			log.Printf("Event [%d] -> %+v data: %#v", i, events[i], events[i].Data)
		}
		if n > 0 {
			break
		}
	}
}
