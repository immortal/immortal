package immortal

import (
	"syscall"
)

func (self *Daemon) Monitor() {
	kq, err := syscall.Kqueue()
	if err != nil {
		self.err <- err
		return
	}

	fd, err := syscall.Open(self.Pidfile, syscall.O_RDONLY, 0)
	if err != nil {
		self.err <- err
		return
	}

	ev1 := syscall.Kevent_t{
		Ident:  uint64(fd),
		Filter: syscall.EVFILT_VNODE,
		Flags:  syscall.EV_ADD | syscall.EV_ENABLE | syscall.EV_ONESHOT,
		Fflags: syscall.NOTE_DELETE | syscall.NOTE_WRITE | syscall.NOTE_EXTEND | syscall.NOTE_ATTRIB | syscall.NOTE_LINK | syscall.NOTE_RENAME | syscall.NOTE_REVOKE,
		Data:   0,
		Udata:  nil,
	}

	var e struct{}
	for {
		events := make([]syscall.Kevent_t, 1)
		n, err := syscall.Kevent(kq, []syscall.Kevent_t{ev1}, events, nil)
		if err != nil {
			self.err <- err
			return
		}
		for i := 0; i < n; i++ {
			self.monitor <- e
			return
		}
	}
}
