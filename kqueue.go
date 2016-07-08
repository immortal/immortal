package immortal

import (
	"syscall"
)

func (self *Daemon) Monitor(pid int) error {
	kq, err := syscall.Kqueue()
	if err != nil {
		return err
	}

	ev1 := syscall.Kevent_t{
		Ident:  uint64(pid),
		Filter: syscall.EVFILT_PROC,
		Flags:  syscall.EV_ADD | syscall.EV_ENABLE | syscall.EV_ONESHOT,
		Fflags: syscall.NOTE_FORK | syscall.NOTE_EXEC | syscall.NOTE_EXIT,
		Data:   0,
		Udata:  nil,
	}

	for {
		events := make([]syscall.Kevent_t, 1)
		n, err := syscall.Kevent(kq, []syscall.Kevent_t{ev1}, events, nil)
		if err != nil {
			return err
		}
		if n > 0 {
			for i := 0; i < n; i++ {
				if events[i].Fflags == syscall.NOTE_FORK {
					self.monitor <- "FORK"
				} else if events[i].Fflags == syscall.NOTE_EXEC {
					self.monitor <- "EXEC"
				} else {
					self.monitor <- "EXIT"
				}
			}
			break
		}
	}
	return nil
}
