// watch -n 0.1 "pgrep -fl run=TestSignals | awk '{print $1}' | xargs -n1 pstree -p "
package immortal

import (
	"io/ioutil"
	"log"
	"os"
	"os/signal"
	"path/filepath"
	"syscall"
	"testing"
	"time"
)

func TestHelperProcessSignals(*testing.T) {
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

func TestSignals(t *testing.T) {
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
	c := make(chan os.Signal)
	wait := make(chan struct{})
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
	}
	println("papa pid: ", os.Getpid())
	d.Run()
	sup := new(Sup)
	go Supervise(sup, d)

	// wait for process to startup
	select {
	case <-wait:
	case <-time.After(1 * time.Second):
		t.Fatal("timeout waiting for pid")
	}
	var testSignals = []struct {
		signal   string
		expected os.Signal
	}{
		{"p", syscall.SIGSTOP},
		{"pause", syscall.SIGSTOP},
		{"s", syscall.SIGSTOP},
		{"stop", syscall.SIGSTOP},
		{"c", syscall.SIGCONT},
		{"cont", syscall.SIGCONT},
		{"h", syscall.SIGHUP},
		{"hup", syscall.SIGHUP},
		{"a", syscall.SIGALRM},
		{"alrm", syscall.SIGALRM},
		{"i", syscall.SIGINT},
		{"int", syscall.SIGINT},
		{"q", syscall.SIGQUIT},
		{"quit", syscall.SIGQUIT},
		{"1", syscall.SIGUSR1},
		{"usr1", syscall.SIGUSR1},
		{"2", syscall.SIGUSR2},
		{"2", syscall.SIGUSR2},
		{"t", syscall.SIGTERM},
		{"term", syscall.SIGTERM},
		{"in", syscall.SIGTTIN},
		{"TTIN", syscall.SIGTTIN},
		{"ou", syscall.SIGTTOU},
		{"out", syscall.SIGTTOU},
		{"TTOU", syscall.SIGTTOU},
		{"w", syscall.SIGWINCH},
		{"winch", syscall.SIGWINCH},
	}
	for _, s := range testSignals {
		d.Control.fifo <- Return{err: nil, msg: s.signal}
		waitSig(t, c, s.expected)
	}

	// test kill process will restart and get new pid
	old_pid := d.process.Pid
	d.Control.fifo <- Return{err: nil, msg: "k"}
	expect(t, d.lock, uint32(1))
	expect(t, d.lock_defer, uint32(0))
	for sup.IsRunning(d.process.Pid) {
		// wait for process to end
	}

	for {
	}
	// send signal "once"
	d.Control.fifo <- Return{err: nil, msg: "o"}
	d.Control.fifo <- Return{err: nil, msg: "k"}
	// process shuld not start
	for d.process.Pid != 0 {
		// wait for process to die
	}
	expect(t, false, sup.IsRunning(d.process.Pid))

	var testSignalsError = []struct {
		signal   string
		expected os.Signal
	}{
		{"p", syscall.SIGILL},
		{"pause", syscall.SIGILL},
		{"s", syscall.SIGILL},
		{"stop", syscall.SIGILL},
		{"c", syscall.SIGILL},
		{"cont", syscall.SIGILL},
		{"h", syscall.SIGILL},
		{"hup", syscall.SIGILL},
		{"a", syscall.SIGILL},
		{"alrm", syscall.SIGILL},
		{"i", syscall.SIGILL},
		{"int", syscall.SIGILL},
		{"q", syscall.SIGILL},
		{"quit", syscall.SIGILL},
		{"1", syscall.SIGILL},
		{"usr1", syscall.SIGILL},
		{"2", syscall.SIGILL},
		{"2", syscall.SIGILL},
		{"t", syscall.SIGILL},
		{"term", syscall.SIGILL},
		{"in", syscall.SIGILL},
		{"TTIN", syscall.SIGILL},
		{"ou", syscall.SIGILL},
		{"out", syscall.SIGILL},
		{"TTOU", syscall.SIGILL},
		{"w", syscall.SIGILL},
		{"winch", syscall.SIGILL},
	}
	for _, s := range testSignalsError {
		d.Control.fifo <- Return{err: nil, msg: s.signal}
		waitSig(t, c, s.expected)
	}

	for {
		println("isolate cmd........")
	}

	// test u
	// bring up the service (new pid expected)
	d.Control.fifo <- Return{err: nil, msg: "u"}
	select {
	case <-wait:
	case <-time.After(2 * time.Second):
		t.Fatal("timeout waiting for pid")
	}

	for d.process.Pid == 0 {
		// wait for new pid
	}

	// test down
	d.Control.fifo <- Return{err: nil, msg: "down"}
	for sup.IsRunning(d.process.Pid) {
		// waiting for process to exit
	}

	// test up
	// bring up the service (new pid expected)
	d.Control.fifo <- Return{err: nil, msg: "up"}
	for sup.IsRunning(d.process.Pid) {
		// want it up
	}
	d.Control.fifo <- Return{err: nil, msg: "once"}
	for d.lock_defer != 1 {
	}
	expect(t, d.lock, uint32(1))
	expect(t, d.lock_defer, uint32(1))

	// send kill (should not start)
	d.Control.fifo <- Return{err: nil, msg: "k"}
	for sup.IsRunning(d.process.Pid) {
	}
	expect(t, false, sup.IsRunning(d.process.Pid))

	// test up
	// bring up the service (new pid expected)
	d.Control.fifo <- Return{err: nil, msg: "u"}
	for !sup.IsRunning(d.process.Pid) {
	}
	old_pid = d.process.Pid

	// send kill (should re-start, and get new pid)
	d.Control.fifo <- Return{err: nil, msg: "k"}
	for sup.IsRunning(d.process.Pid) {
	}

	select {
	case <-wait:
	case <-time.After(2 * time.Second):
		t.Fatal("timeout waiting for pid")
	}
	for old_pid == d.process.Pid {
	}

	// should be running
	expect(t, true, sup.IsRunning(d.process.Pid))

	// quit
	d.Control.fifo <- Return{err: nil, msg: "k"}
	d.Control.fifo <- Return{err: nil, msg: "exit"}
}

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
