package immortal

import (
	"io/ioutil"
	"log"
	"os"
	"strings"
	"sync"
	"testing"
	"time"
)

func TestNewStderrLogger(t *testing.T) {
	tmpfile, err := ioutil.TempFile("", "stderr")
	if err != nil {
		log.Fatal(err)
	}
	defer os.Remove(tmpfile.Name())
	log.SetOutput(ioutil.Discard)
	cfg := &Config{
		Stderr: Log{
			File: tmpfile.Name(),
		},
	}
	l := NewStderrLogger(cfg)
	expect(t, true, l != nil)
}

func TestNewStderrLoggerFileNone(t *testing.T) {
	log.SetOutput(ioutil.Discard)
	cfg := &Config{
		Stderr: Log{
			File: "/dev/null/nonexist",
		},
	}
	l := NewStderrLogger(cfg)
	expect(t, true, l == nil)
}

func TestNewLoggerFileNone(t *testing.T) {
	log.SetOutput(ioutil.Discard)
	cfg := &Config{
		Log: Log{
			File: "/dev/null/nonexist",
		},
	}
	quit := make(chan struct{})
	l := NewLogger(cfg, quit)
	expect(t, true, l == nil)
}

func TestNewLoggerBadLogger(t *testing.T) {
	log.SetOutput(ioutil.Discard)
	cfg := &Config{
		Logger: "any-logger",
	}
	quit := make(chan struct{})
	l := NewLogger(cfg, quit)
	expect(t, true, l == nil)
}

func TestNewLoggerLogger(t *testing.T) {
	cfg := &Config{
		Logger: "cat",
	}
	quit := make(chan struct{})
	l := NewLogger(cfg, quit)
	expect(t, true, l != nil)
	close(quit)
}

func TestNewLoggerKilled(t *testing.T) {
	cfg := &Config{
		Logger: "sleep 300",
	}
	quit := make(chan struct{})
	l := NewLogger(cfg, quit)
	expect(t, true, l != nil)
	var wg sync.WaitGroup
	wg.Add(1)
	go func() {
		time.Sleep(time.Second)
		close(quit)
		wg.Done()
	}()
	wg.Wait()
	// TODO verify sleep process is killed
}

func TestNewLoggerRetry(t *testing.T) {
	var mylog myBuffer
	log.SetOutput(&mylog)
	log.SetFlags(0)
	cfg := &Config{
		Logger: "false",
	}
	quit := make(chan struct{})
	l := NewLogger(cfg, quit)
	expect(t, true, l != nil)
	var wg sync.WaitGroup
	wg.Add(1)
	go func() {
		time.Sleep(2 * time.Second)
		close(quit)
		wg.Done()
	}()
	wg.Wait()
	m := strings.Split(mylog.String(), "\n")
	expect(t, true, len(m) > 1)
}
