package immortal

import (
	"os"
	"os/signal"
	"syscall"
	"testing"
	"time"
)

func waitSig(t *testing.T, c <-chan os.Signal, sig os.Signal) {
	select {
	case s := <-c:
		if s != sig {
			t.Fatalf("signal was %v, want %v", s, sig)
		}
	case <-time.After(1 * time.Second):
		t.Fatalf("timeout waiting for %v", sig)
	}
}

func TestHelperProcess(t *testing.T) {
	if os.Getenv("GO_WANT_HELPER_PROCESS") != "1" {
		return
	}
	c := make(chan os.Signal, 1)
	signal.Notify(c, syscall.SIGHUP, syscall.SIGUSR1)
	waitSig(t, c, syscall.SIGHUP)
}

func TestSignals(t *testing.T) {
	os.Setenv("GO_WANT_HELPER_PROCESS", "1")
	p := ""
	ctrl := true
	D, e := New(nil, &p, &p, &p, &p, &p, &p, &p, []string{"go", "test", "-run", "TestHelperProcess"}, &ctrl)
	if e != nil {
		t.Error(e)
	}
	D.Control()
	go D.Supervise()
	D.handleSignals("o", D.ctrl.state)
	if D.count_defer != 1 {
		t.Errorf("Expecting: 1 got %v", D.count_defer)
	}
}
