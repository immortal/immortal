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
		file, w io.WriteCloser
	)

	ch = make(chan error)

	// create a multiwriter
	multi := multiwriter.New()
	var m *multiwriter.MultiWriter = multi.(*multiwriter.MultiWriter)

	if self.run.Logfile != "" {
		self.log = true
		file, err = logrotate.New(self.run.Logfile)
		if err != nil {
			log.Printf("Failed to open log file %q: %s\n", self.run.Logfile, err)
			return
		}
		m.Append(file)
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
		self.log = true
		runLogger()

		go func() {
			for {
				select {
				case err = <-ch:
					log.Print("logger exited ", err.Error())
					m.Remove(w)
					time.Sleep(time.Second)
					runLogger()
					m.Append(w)
				}
			}
		}()
		m.Append(w)
	}

	// create the logger
	if self.log {
		self.logger = log.New(multi, "", 0)
	}
}
