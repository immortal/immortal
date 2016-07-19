package immortal

import (
	"github.com/immortal/logrotate"
	"github.com/immortal/multiwriter"
	"io"
	"log"
	"os/exec"
	"strings"
	"time"
)

func (self *Daemon) Log(msg string) {
	self.logger.Print(msg)
}

func (self *Daemon) Logger() {
	var (
		ch      chan error
		err     error
		multi   io.Writer
		file, w io.WriteCloser
	)

	ch = make(chan error)

	if self.run.Logfile != "" {
		file, err = logrotate.New(self.run.Logfile)
		if err != nil {
			log.Printf("Failed to open log file %q: %s\n", self.run.Logfile, err)
			return
		}
	}

	runLogger := func() {
		command := strings.Fields(self.run.Logger)
		cmd := exec.Command(command[0], command[1:]...)
		w, err = cmd.StdinPipe()
		if err != nil {
			log.Printf("logger PIPE error: %s", err)
			ch <- err
			return
		}
		go func() {
			if err := cmd.Start(); err != nil {
				ch <- err
			}
			ch <- cmd.Wait()
		}()
	}

	if self.run.Logger != "" {
		runLogger()

		go func() {
			for {
				select {
				case err = <-ch:
					log.Print("logger exited ", err.Error())
					time.Sleep(time.Second)
					runLogger()
					multi = multiwriter.New(file, w)
					self.logger = log.New(multi, "", 0)
				}
			}
		}()
		multi = multiwriter.New(file, w)
	} else {
		multi = multiwriter.New(file)
	}

	// create the logger
	self.logger = log.New(multi, "", 0)
}
