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
		mw      *multiwriter.MultiWriter
	)

	// create a multiwriter
	multi := multiwriter.New()
	mw = multi.(*multiwriter.MultiWriter)

	if cfg.Log.File != "" {
		if file, err = logrotate.New(cfg.Log.File, cfg.Log.Age, cfg.Log.Num, cfg.Log.Size, cfg.Log.Timestamp); err != nil {
			log.Printf("Failed to open log file %q: %s\n", cfg.Log.File, err)
		} else {
			mw.Append(file)
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
						mw.Remove(w)
						time.Sleep(time.Second)
						if err := runLogger(); err == nil {
							mw.Append(w)
						}
					}
				}
			}()

			mw.Append(w)
		}
	}

	// create the logger
	if mw.Len() > 0 {
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
