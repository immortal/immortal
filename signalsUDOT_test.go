package immortal

import (
	"fmt"
	"os"
	"os/signal"
	"path/filepath"
	"testing"
	"time"
)

func TestHelperProcessSignalsUDOT(*testing.T) {
	if os.Getenv("GO_WANT_HELPER_PROCESS") != "1" {
		return
	}
	c := make(chan os.Signal, 1)
	signal.Notify(c, os.Interrupt, os.Kill)
	select {
	case <-c:
		os.Exit(1)
	case <-time.After(10 * time.Second):
		os.Exit(0)
	}
}

func TestSignalsUDOT(t *testing.T) {
	//log.SetOutput(ioutil.Discard)
	base := filepath.Base(os.Args[0]) // "exec.test"
	dir := filepath.Dir(os.Args[0])   // "/tmp/go-buildNNNN/os/exec/_test"
	if dir == "." {
		t.Skip("skipping; running test at root somehow")
	}
	parentDir := filepath.Dir(dir) // "/tmp/go-buildNNNN/os/exec"
	dirBase := filepath.Base(dir)  // "_test"
	if dirBase == "." {
		t.Skipf("skipping; unexpected shallow dir of %q", dir)
	}
	cfg := &Config{
		Env:     map[string]string{"GO_WANT_HELPER_PROCESS": "1"},
		command: []string{filepath.Join(dirBase, base), "-test.run=TestHelperProcessSignalsUDOT"},
		Cwd:     parentDir,
		Pid: Pid{
			Parent: filepath.Join(parentDir, "parent.pid"),
			Child:  filepath.Join(parentDir, "child.pid"),
		},
	}
	d, err := New(cfg)
	if err != nil {
		t.Fatal(err)
	}
	d.Run()
	sup := new(Sup)

	old_pid := d.Process().Pid
	// test "k", process should restart and get a new pid
	//d.Control.fifo <- Return{err: nil, msg: "k"}
	fmt.Printf("d.Process().Pid = %+v\n", d.Process().Pid)
	sup.HandleSignals("k", d)
	expect(t, uint32(1), d.lock)
	expect(t, uint32(0), d.lock_defer)
	done := make(chan struct{}, 1)
	select {
	case <-d.Control.state:
		d.cmd.Process.Pid = 0
		done <- struct{}{}
	}
	select {
	case <-done:
		d.Run()
	}

	if old_pid == d.Process().Pid {
		t.Fatal("Expecting a new pid")
	}

	// test "d", (keep it down and don't restart)
	sup.HandleSignals("d", d)
	select {
	case <-d.Control.state:
		d.cmd.Process.Pid = 0
		done <- struct{}{}
	}
	select {
	case <-done:
		d.Run()
	}
	expect(t, false, d.IsRunning())
	expect(t, 0, d.Process().Pid)

	// test "u" more debug with: watch -n 0.1 "pgrep -fl run=TestSignals | awk '{print $1}' | xargs -n1 pstree -p "
	sup.HandleSignals("u", d)

	// test "once", process should not restart after going down
	sup.HandleSignals("o", d)
	sup.HandleSignals("k", d)
	select {
	case <-d.Control.state:
		d.cmd.Process.Pid = 0
		done <- struct{}{}
	}
	select {
	case <-done:
		d.Run()
	}
	expect(t, false, d.IsRunning())

	// test "up"
	sup.HandleSignals("up", d)
	sup.HandleSignals("stop", d)
	sup.HandleSignals("cont", d)
	sup.HandleSignals("t", d)
	select {
	case <-d.Control.state:
		d.cmd.Process.Pid = 0
		done <- struct{}{}
	}
	select {
	case <-done:
		d.Run()
	}
	// after exiting will get a race cond
	expect(t, true, d.IsRunning())
	sup.HandleSignals("exit", d)
}
