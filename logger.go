package immortal

import (
	"bufio"
	"github.com/immortal/logrotate"
	"github.com/immortal/multiwriter"
	"io"
	"log"
	"os/exec"
	"strings"
	"time"
)

type Logger interface {
	StdHandler(input io.ReadCloser)
}

type LogWriter struct {
	logger *log.Logger
}

func NewLogger(cfg *Config) *log.Logger {
	var (
		ch      chan error
		err     error
		file, w io.WriteCloser
	)

	ch = make(chan error)

	// create a multiwriter
	multi := multiwriter.New()
	var m *multiwriter.MultiWriter = multi.(*multiwriter.MultiWriter)

	if cfg.Log.File != "" {
		cfg.log = true
		file, err = logrotate.New(cfg.Log.File)
		if err != nil {
			log.Printf("Failed to open log file %q: %s\n", cfg.Log.File, err)
		}
		m.Append(file)
	}

	runLogger := func() {
		command := strings.Fields(cfg.Logger)
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

	if cfg.Logger != "" {
		cfg.log = true
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
	if cfg.log {
		return log.New(multi, "", 0)
	}
	return nil
}

func (self *LogWriter) StdHandler(input io.ReadCloser) {
	in := bufio.NewScanner(input)
	for in.Scan() {
		self.logger.Print(in.Text())
	}
	input.Close()
}
