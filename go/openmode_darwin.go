// +build darwin

package immortal

import "syscall"

const (
	openModeDir  = syscall.O_EVTONLY | syscall.O_DIRECTORY
	openModeFile = syscall.O_EVTONLY
)
