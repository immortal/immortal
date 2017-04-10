// +build freebsd netbsd openbsd dragonfly darwin
// +build amd64

package immortal

import (
	"os"
	"syscall"
)

// WatchFile check for changes on a file via kqueue EVFILT_VNODE
func WatchFile(f string, ch chan<- string) error {
	file, err := os.Open(f)
	if err != nil {
		return err
	}

	kq, err := syscall.Kqueue()
	if err != nil {
		return err
	}

	// NOTE_WRITE and NOTE_ATTRIB returns twice, removing NOTE_ATTRIB (touch) will not work
	ev1 := syscall.Kevent_t{
		Ident:  uint64(file.Fd()),
		Filter: syscall.EVFILT_VNODE,
		Flags:  syscall.EV_ADD | syscall.EV_ENABLE | syscall.EV_ONESHOT,
		Fflags: syscall.NOTE_DELETE | syscall.NOTE_WRITE | syscall.NOTE_LINK | syscall.NOTE_RENAME | syscall.NOTE_REVOKE,
		Data:   0,
		Udata:  nil,
	}

	// create kevent
	events := []syscall.Kevent_t{ev1}
	n, err := syscall.Kevent(kq, events, events, nil)
	if err != nil {
		return err
	}

	// wait for an event
	for {
		if n > 0 {
			file.Close()
			ch <- f
			return nil
		}
	}
}
