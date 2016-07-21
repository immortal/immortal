package immortal

import (
	"os/exec"
	"testing"
)

func TestWatchPid0(t *testing.T) {
	D := &Daemon{}
	ch := make(chan error, 1)
	D.watchPid(0, ch)
	err := <-ch
	if err != nil {
		if err.Error() != "PID NOT FOUND" {
			t.Error(err)
		}
	}
}

func TestWatchPidGetpid(t *testing.T) {
	cmd := exec.Command("go", "version")
	cmd.Start()
	pid := cmd.Process.Pid
	D := &Daemon{}
	ch := make(chan error, 1)
	D.watchPid(pid, ch)
	err := cmd.Wait()
	if err != nil {
		t.Error(err)
	}
	err = <-ch
	if err != nil {
		if err.Error() != "EXIT" {
			t.Error(err)
		}
	}
}

func TestWatchPidGetpidKill(t *testing.T) {
	D := &Daemon{}
	ch := make(chan error)

	cmd := exec.Command("sleep", "100")
	cmd.Start()
	pid := cmd.Process.Pid
	go func() {
		D.watchPid(pid, ch)
		ch <- cmd.Wait()
	}()

	if err := cmd.Process.Kill(); err != nil {
		t.Errorf("failed to kill: %s", err)
	}
	select {
	case err := <-ch:
		if err != nil {
			if err.Error() != "EXIT" {
				t.Error(err)
			}
		}
	}
}
