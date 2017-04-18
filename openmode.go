// +build freebsd netbsd openbsd dragonfly

package immortal

import "syscall"

const openMode = syscall.O_NONBLOCK | syscall.O_RDONLY
