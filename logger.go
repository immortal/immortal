package immortal

import (
	"bufio"
	"io"
	"log"
	"os/exec"
	"strings"
	"time"

	"github.com/immortal/logrotate"
	"github.com/immortal/multiwriter"
)

type Logger interface {
	Log(input io.ReadCloser)
	IsLogging() bool
}

type LogWriter struct {
	logger *log.Logger
}

func NewLogger(cfg *Config, quit chan struct{}) *log.Logger {
	var (
		ch      chan error
		err     error
		file, w io.WriteCloser
	)

	// create a multiwriter
	multi := multiwriter.New()
	var m *multiwriter.MultiWriter = multi.(*multiwriter.MultiWriter)

	if cfg.Log.File != "" {
		file, err = logrotate.New(cfg.Log.File)
		if err != nil {
			log.Printf("Failed to open log file %q: %s\n", cfg.Log.File, err)
		} else {
			m.Append(file)
		}
	}

	ch = make(chan error)
	runLogger := func() {
		command := strings.Fields(cfg.Logger)
		cmd := exec.Command(command[0], command[1:]...)
		w, err = cmd.StdinPipe()
		if err != nil {
			log.Printf("logger pipe error: %s", err)
			return
		}
		go func() {
			if err := cmd.Start(); err != nil {
				log.Printf("logger error: %s", err)
				return
			}
			ch <- cmd.Wait()
		}()
	}

	if cfg.Logger != "" {
		runLogger()

		// keep logger up and running
		go func(quit chan struct{}) {
			for {
				select {
				case <-quit:
					w.Close()
					return
				case err := <-ch:
					log.Printf("logger %s %v", err, time.Now())
					m.Remove(w)
					time.Sleep(time.Second)
					runLogger()
					m.Append(w)
				}
			}
		}(quit)

		m.Append(w)
	}

	// create the logger
	if m.Len() > 0 {
		return log.New(multi, "", 0)
	}
	return nil
}

func (l *LogWriter) Log(input io.ReadCloser) {
	in := bufio.NewScanner(input)
	for in.Scan() {
		l.logger.Print(in.Text())
	}
	input.Close()
}

func (l *LogWriter) IsLogging() bool {
	return l.logger != nil
}
