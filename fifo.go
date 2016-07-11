package immortal

import (
	"bufio"
	"os"
)

func (self *Daemon) FIFO() {

	//	syscall.Mknod(os.Args[1], syscall.S_IFIFO|0666, 0)
	file, err := os.OpenFile(os.Args[1], os.O_RDONLY, os.ModeNamedPipe)
	if err != nil {
		panic(err)
	}
	defer file.Close()

	scanner := bufio.NewScanner(file)
	for scanner.Scan() {
		fmt.Println(scanner.Text())
	}

}
