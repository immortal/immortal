package immortal

import (
	"bufio"
	"fmt"
	"os"
	"syscall"
)

func (self *Daemon) FIFO() error {
	s_dir := fmt.Sprintf("%s/supervise", self.run.Cwd)
	err := os.Mkdir(s_dir, 0700)
	if err != nil {
		if os.IsNotExist(err) {
			return err
		}
	}

	// create status pipe
	status_fifo := fmt.Sprintf("%s/status", s_dir)
	syscall.Mknod(status_fifo, syscall.S_IFIFO|0666, 0)

	file, err := os.OpenFile(s_dir+"/status", os.O_RDWR, os.ModeNamedPipe)
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
