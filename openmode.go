// +build freebsd netbsd openbsd dragonfly

package immortal

import "syscall"

const (
	openModeDir  = syscall.O_NONBLOCK | syscall.O_RDONLY | syscall.O_DIRECTORY
	openModeFile = syscall.O_NONBLOCK | syscall.O_RDONLY
)
