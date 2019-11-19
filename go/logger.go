package immortal

import (
	"bufio"
	"context"
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

// Log write to the logger
func (l *LogWriter) Log(input io.ReadCloser) {
	for {
		in := bufio.NewScanner(input)
		for in.Scan() {
			l.logger.Print(in.Text())
		}
		if in.Err() == io.EOF || in.Err() == nil {
			break
		}
		// If non EOF error happens, we dump the log and not to close the pipe.
		// Calling input.Close() right away may trigger SIGPIPE signal to the daemon and restart.
		l.logger.Printf("immortal failed to scan log input: %v", in.Err())
	}
	input.Close()
}

// IsLogging return true if an availale logger exists
func (l *LogWriter) IsLogging() bool {
	return l.logger != nil
}

// NewStderrLogger return  Logger instance
func NewStderrLogger(cfg *Config) *log.Logger {
	if cfg.Stderr.File != "" {
		file, err := logrotate.New(cfg.Stderr.File, cfg.Stderr.Age, cfg.Stderr.Num, cfg.Stderr.Size, cfg.Stderr.Timestamp)
		if err != nil {
			log.Printf("Failed to open log file %q: %s\n", cfg.Stderr.File, err)
			return nil
		}
		return log.New(file, "", 0)
	}
	return nil
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
		ctx, cancel := context.WithCancel(context.Background())

		runLogger := func() error {
			command := strings.Fields(cfg.Logger)
			cmd := exec.CommandContext(ctx, command[0], command[1:]...)
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
						cancel()
						return
					case err := <-ch:
						log.Printf("logger %v", err)
						mw.Remove(w)
						time.Sleep(time.Second)
						if err := runLogger(); err == nil {
							mw.Append(w)
						}
					}
				}
			}()

			// add writer
			mw.Append(w)
		}
	}

	// create the logger
	if mw.Len() > 0 {
		return log.New(multi, "", 0)
	}
	return nil
}
