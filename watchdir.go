// +build freebsd netbsd openbsd dragonfly darwin
// +build amd64

package immortal

import (
	"os"
	"syscall"
)

// WatchDir check for changes on a directory via Kqueue EVFILT_VNODE
func WatchDir(dir string, ch chan<- struct{}) error {
	file, err := os.Open(dir)
	if err != nil {
		return err
	}

	kq, err := syscall.Kqueue()
	if err != nil {
		return err
	}

	ev1 := syscall.Kevent_t{
		Ident:  uint64(file.Fd()),
		Filter: syscall.EVFILT_VNODE,
		Flags:  syscall.EV_ADD | syscall.EV_ENABLE | syscall.EV_ONESHOT,
		Fflags: syscall.NOTE_DELETE | syscall.NOTE_WRITE | syscall.NOTE_ATTRIB | syscall.NOTE_LINK | syscall.NOTE_RENAME | syscall.NOTE_REVOKE,
		Data:   0,
		Udata:  nil,
	}

	// create kevent
	events := make([]syscall.Kevent_t, 1)
	n, err := syscall.Kevent(kq, []syscall.Kevent_t{ev1}, events, nil)
	if err != nil {
		return err
	}

	// wait for an event
	for {
		if n > 0 {
			file.Close()
			ch <- struct{}{}
			return nil
		}
	}
}
