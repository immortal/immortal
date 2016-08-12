package immortal

// watch -n 0.1 "pgrep -fl run=TestSignals | awk '{print $1}' | xargs -n1 pstree -p "

import (
	"io/ioutil"
	"log"
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
	log.SetOutput(ioutil.Discard)
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
		command: []string{filepath.Join(dirBase, base), "-test.run=TestHelperProcessSignals"},
		Cwd:     parentDir,
		Pid: Pid{
			Parent: filepath.Join(parentDir, "parent.pid"),
			Child:  filepath.Join(parentDir, "child.pid"),
		},
	}
	d := &Daemon{
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
		process: &os.Process{},
	}
	d.Run()
	sup := new(Sup)
	go Supervise(sup, d)

	for d.process.Pid == 0 {
		// wait for process to come up
	}

	// test "k", process should restart and get a new pid
	old_pid := d.process.Pid
	d.Control.fifo <- Return{err: nil, msg: "k"}
	expect(t, d.lock, uint32(1))
	expect(t, d.lock_defer, uint32(0))
	for old_pid != d.process.Pid && d.process.Pid == 0 {
		// wait for process to restart and come up
	}
	expect(t, true, sup.IsRunning(d.process.Pid))

	// test "once", process should not restart after going down
	old_pid = d.process.Pid
	d.Control.fifo <- Return{err: nil, msg: "o"}
	d.Control.fifo <- Return{err: nil, msg: "k"}
	// process shuld not start
	for d.process.Pid != 0 {
		// wait for process to restart and came up
	}
	expect(t, false, sup.IsRunning(d.process.Pid))

	// test "u" bring up the service (new pid expected)
	d.Control.fifo <- Return{err: nil, msg: "u"}
	for d.process.Pid == 0 {
		// wait for new pid
	}
	expect(t, true, sup.IsRunning(d.process.Pid))

	// test "down"
	d.Control.fifo <- Return{err: nil, msg: "down"}
	for d.process.Pid != 0 {
		// wait for new pid
	}
	expect(t, false, sup.IsRunning(d.process.Pid))

	// test "up" bring up the service
	d.Control.fifo <- Return{err: nil, msg: "up"}
	for d.process.Pid == 0 {
		// wait for new pid
	}
	expect(t, true, sup.IsRunning(d.process.Pid))

	d.Control.fifo <- Return{err: nil, msg: "once"}
	d.Control.fifo <- Return{err: nil, msg: "t"}
	for d.process.Pid != 0 {
		// wait for process to stop
	}

	expect(t, false, sup.IsRunning(d.process.Pid))
	d.Control.fifo <- Return{err: nil, msg: "exit"}
}
