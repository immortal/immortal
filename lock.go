package immortal

import (
	"fmt"
	"log"
	"os"
	"syscall"
)

func (self *Daemon) Lock() error {
	lock_file := fmt.Sprintf("%s/lock", self.sdir)
	file, err := os.Create(lock_file)
	if err != nil {
		log.Println("supervise dir: ", self.sdir, "error: ", err.Error())
		return err
	}
	return syscall.Flock(int(file.Fd()), syscall.LOCK_EX+syscall.LOCK_NB)
}
