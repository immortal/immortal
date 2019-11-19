// +build freebsd netbsd openbsd darwin
// +build arm

package immortal

import "syscall"

// WatchDir check for changes on a directory via Kqueue EVFILT_VNODE
func WatchDir(dir string, ch chan<- string) error {
	watchfd, err := syscall.Open(dir, openModeDir, 0700)
	if err != nil {
		return err
	}

	kq, err := syscall.Kqueue()
	if err != nil {
		syscall.Close(watchfd)
		return err
	}

	ev1 := syscall.Kevent_t{
		Ident:  uint32(watchfd),
		Filter: syscall.EVFILT_VNODE,
		Flags:  syscall.EV_ADD | syscall.EV_ENABLE | syscall.EV_ONESHOT,
		Fflags: syscall.NOTE_WRITE | syscall.NOTE_ATTRIB,
		Data:   0,
	}

	for {
		// create kevent
		kevents := []syscall.Kevent_t{ev1}
		n, err := syscall.Kevent(kq, kevents, kevents, nil)
		if err != nil {
			syscall.Close(watchfd)
			return err
		}

		// wait for an event
		for len(kevents) > 0 {
			if n > 0 {
				ch <- dir
			}
			// Move to next event
			kevents = kevents[1:]
		}
	}
}

// WatchFile check for changes on a file via kqueue EVFILT_VNODE
func WatchFile(f string, ch chan<- string) error {
	watchfd, err := syscall.Open(f, openModeFile, 0700)
	if err != nil {
		return err
	}

	kq, err := syscall.Kqueue()
	if err != nil {
		syscall.Close(watchfd)
		return err
	}

	// NOTE_WRITE and NOTE_ATTRIB returns twice, if removing NOTE_ATTRIB (touch) will not work
	ev1 := syscall.Kevent_t{
		Ident:  uint32(watchfd),
		Filter: syscall.EVFILT_VNODE,
		Flags:  syscall.EV_ADD | syscall.EV_ENABLE | syscall.EV_ONESHOT,
		Fflags: syscall.NOTE_DELETE | syscall.NOTE_WRITE | syscall.NOTE_ATTRIB | syscall.NOTE_LINK | syscall.NOTE_RENAME | syscall.NOTE_REVOKE,
		Data:   0,
	}

	// create kevent
	kevents := []syscall.Kevent_t{ev1}
	n, err := syscall.Kevent(kq, kevents, kevents, nil)
	if err != nil {
		syscall.Close(watchfd)
		return err
	}

	// wait for an event
	for len(kevents) > 0 {
		if n > 0 {
			// do something
		}
		// Move to next event
		kevents = kevents[1:]
	}
	syscall.Close(watchfd)
	ch <- f
	return nil
}
