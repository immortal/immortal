// +build freebsd netbsd openbsd dragonfly darwin

package immortal

import (
	"os"
	"syscall"
)

func (self *Daemon) watchPid(ch chan<- Return) {
	kq, err := syscall.Kqueue()
	if err != nil {
		ch <- Return{os.NewSyscallError("kqueue", err), ""}
		return
	}

	ev1 := syscall.Kevent_t{
		Ident:  uint64(self.pid),
		Filter: syscall.EVFILT_PROC,
		Flags:  syscall.EV_ADD | syscall.EV_ENABLE | syscall.EV_ONESHOT,
		Fflags: syscall.NOTE_EXIT,
		Data:   0,
		Udata:  nil,
	}

	for {
		events := make([]syscall.Kevent_t, 1)
		n, err := syscall.Kevent(kq, []syscall.Kevent_t{ev1}, events, nil)
		if err != nil {
			ch <- Return{os.NewSyscallError("kqueue", err), ""}
			return
		}
		for i := 0; i < n; i++ {
			syscall.Close(kq)
			ch <- Return{nil, "EXIT"}
			return
		}
	}
}
