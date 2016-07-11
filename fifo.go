package immortal

import (
	"bufio"
	"fmt"
	"os"
	"syscall"
)

func (self *Daemon) FIFO() error {
	// create status pipe
	status_fifo := fmt.Sprintf("%s/status", self.sdir)
	syscall.Mknod(status_fifo, syscall.S_IFIFO|0666, 0)

	file, err := os.OpenFile(self.sdir+"/status", os.O_RDWR, os.ModeNamedPipe)
	if err != nil {
		return err
	}

	reader := bufio.NewReader(file)

	go func() {
		defer file.Close()
		for {
			text, err := reader.ReadString('\n')
			Log(Green(fmt.Sprintf("%v - %v", text, err)))
		}
	}()

	return nil
}
