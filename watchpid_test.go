package immortal

import (
	"os/exec"
	"syscall"
	"testing"
	"time"
)

func TestWatchPidGetpid(t *testing.T) {
	ch := make(chan error, 1)
	d := &Daemon{}
	cmd := exec.Command("go", "version")
	cmd.Start()
	pid := cmd.Process.Pid
	go func() {
		d.WatchPid(pid, ch)
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
	d := &Daemon{}
	ch := make(chan error, 1)
	cmd := exec.Command("sleep", "100")
	cmd.Start()
	pid := cmd.Process.Pid
	go func() {
		d.WatchPid(pid, ch)
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
