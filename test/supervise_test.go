package immortal

import (
	"testing"
)

func TestSupervise(t *testing.T) {
	D := &Daemon{
		command: []string{"sleep", "1000"},
		ctrl: Ctrl{
			quit:  make(chan struct{}),
			state: make(chan error),
		},
	}
	go func() {
		close(D.ctrl.quit)
	}()
	D.Supervise()
}
