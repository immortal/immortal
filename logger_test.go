package immortal

import (
	"io/ioutil"
	"log"
	"os"
	"testing"
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
