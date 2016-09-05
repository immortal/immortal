package immortal

import (
	"bufio"
	"io"
	"log"
	"os"
	"os/exec"
	"os/signal"
	"strings"
	"syscall"
	"time"

	"github.com/immortal/logrotate"
	"github.com/immortal/multiwriter"
)

// Logger interface
type Logger interface {
	Log(input io.ReadCloser)
	IsLogging() bool
}

// LogWriter implements Logger
type LogWriter struct {
	logger *log.Logger
}

// NewLogger return a Logger instance
func NewLogger(cfg *Config, quit chan struct{}) *log.Logger {
	var (
		err     error
		file, w io.WriteCloser
	)

	// create a multiwriter
	multi := multiwriter.New()
	var m *multiwriter.MultiWriter = multi.(*multiwriter.MultiWriter)

	if cfg.Log.File != "" {
		lr, err := logrotate.New(cfg.Log.File, cfg.Log.Age, cfg.Log.Num, cfg.Log.Size)
		if err != nil {
			log.Printf("Failed to open log file %q: %s\n", cfg.Log.File, err)
		} else {
			// rotate logs when receiving an USR1
			c := make(chan os.Signal, 1)
			signal.Notify(c, syscall.SIGUSR1)
			go func() {
				for {
					<-c
					lr.Rotate()
				}
			}()
			file = lr
			m.Append(file)
		}
	}

	if cfg.Logger != "" {

		ch := make(chan error)

		runLogger := func() error {
			command := strings.Fields(cfg.Logger)
			cmd := exec.Command(command[0], command[1:]...)
			w, err = cmd.StdinPipe()
			if err != nil {
				return err
			}
			if err := cmd.Start(); err != nil {
				return err
			}
			go func() {
				ch <- cmd.Wait()
			}()
			return nil
		}

		if err := runLogger(); err != nil {
			log.Printf("logger error: %s", err)
		} else {

			// keep logger up
			go func() {
				for {
					select {
					case <-quit:
						w.Close()
						return
					case err := <-ch:
						log.Printf("logger %s", err)
						m.Remove(w)
						time.Sleep(time.Second)
						if err := runLogger(); err == nil {
							m.Append(w)
						}
					}
				}
			}()

			m.Append(w)
		}
	}

	// create the logger
	if m.Len() > 0 {
		return log.New(multi, "", 0)
	}
	return nil
}

// Log write to the logger
func (l *LogWriter) Log(input io.ReadCloser) {
	in := bufio.NewScanner(input)
	for in.Scan() {
		l.logger.Print(in.Text())
	}
	input.Close()
}

// IsLogging return true if an availale logger exists
func (l *LogWriter) IsLogging() bool {
	return l.logger != nil
}
