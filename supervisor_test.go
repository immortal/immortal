package immortal

import (
	//	"fmt"
	"os"
	"os/signal"
	"syscall"
	"testing"
	//	"time"
)

type myFork struct{}

func (self myFork) Fork() {
	return
}

func TestHelperProcess(t *testing.T) {
	if os.Getenv("GO_WANT_HELPER_PROCESS") != "1" {
		return
	}
	c := make(chan os.Signal, 1)
	signal.Notify(c, syscall.SIGHUP, syscall.SIGUSR1)
	select {
	case s := <-c:
		if s != syscall.SIGHUP {
			expect(t, syscall.SIGHUP, s)
		}
	}
}

func TestSupervisor(t *testing.T) {
	os.Setenv("GO_WANT_HELPER_PROCESS", "1")
	cfg := &Config{
		command: []string{"go", "test", "-run", "TestHelperProcess"},
	}
	d := Daemon{
		Config: cfg,
		Control: &Control{
			fifo:  make(chan Return),
			quit:  make(chan struct{}),
			state: make(chan error),
		},
		Forker: &myFork{},
		Logger: &LogWriter{
			logger: NewLogger(cfg),
		},
	}
	d.Run()
	//	sup := new(Sup)
	//	sup.HandleSignals("h", &d)
	//	time.Sleep(1000 * time.Second)
}
