// +build freebsd netbsd openbsd dragonfly darwin
// +build amd64

package immortal

import (
	"fmt"
	"os"
	"syscall"
)

// WatchPid check pid changes
func (d *Daemon) WatchPid(pid int, ch chan<- error) {
	if !d.IsRunning(pid) {
		ch <- fmt.Errorf("PID NOT FOUND")
		return
	}

	kq, err := syscall.Kqueue()
	if err != nil {
		ch <- os.NewSyscallError("kqueue", err)
		return
	}

	ev1 := syscall.Kevent_t{
		Ident:  uint64(pid),
		Filter: syscall.EVFILT_PROC,
		Flags:  syscall.EV_ADD | syscall.EV_ENABLE | syscall.EV_ONESHOT,
		Fflags: syscall.NOTE_EXIT,
		Data:   0,
	}

	// create kevent
	events := []syscall.Kevent_t{ev1}
	n, err := syscall.Kevent(kq, events, events, nil)
	if err != nil {
		ch <- os.NewSyscallError("kqueue", err)
		return
	}

	for {
		if n > 0 {
			syscall.Close(kq)
			ch <- fmt.Errorf("EXIT")
			return
		}
	}
}
