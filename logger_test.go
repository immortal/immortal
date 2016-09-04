package immortal

import (
	"io/ioutil"
	"log"
	"testing"
)

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
