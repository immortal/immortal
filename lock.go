package immortal

import (
	"fmt"
	"os"
	"syscall"
)

func (self *Daemon) Lock() error {
	lock_file := fmt.Sprintf("%s/supervise/lock", self.sdir)
	file, err := os.Create(lock_file)
	if err != nil {
		return err
	}
	return syscall.Flock(int(file.Fd()), syscall.LOCK_EX)
}
