package immortal

import (
	"os/exec"
	"syscall"
	"testing"
	"time"
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
	D := &Daemon{}
	ch := make(chan error)

	cmd := exec.Command("go", "version")
	cmd.Start()
	pid := cmd.Process.Pid
	go func() {
		D.watchPid(pid, ch)
		ch <- cmd.Wait()
	}()
	select {
	case <-time.After(time.Millisecond):
		syscall.Kill(pid, syscall.SIGTERM)
	case err := <-ch:
		if err != nil {
			if err.Error() != "EXIT" {
				t.Error(err)
			}
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

	select {
	case err := <-ch:
		if err != nil {
			if err.Error() != "EXIT" {
				t.Error(err)
			}
		}
	case <-time.After(1 * time.Millisecond):
		if err := cmd.Process.Kill(); err != nil {
			t.Errorf("failed to kill: %s", err)
		}
	}
}
