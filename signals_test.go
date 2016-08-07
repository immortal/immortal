package immortal

import (
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
	<-c
	os.Exit(0)
}

func TestSignals(t *testing.T) {
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
		process: &catchSignals{&os.Process{}, c, wait},
	}
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

	time.Sleep(time.Second)
	// test kill process will restart
	old_pid := d.process.GetPid()
	d.Control.fifo <- Return{err: nil, msg: "k"}
	expect(t, d.count, uint32(1))
	expect(t, d.count_defer, uint32(0))

	// wait for process to came up and then send signal "once"
	for sup.IsRunning(d.process.GetPid()) {
		d.Control.fifo <- Return{err: nil, msg: "o"}
		break
	}
	expect(t, d.count, uint32(1))
	expect(t, d.count_defer, uint32(1))
	expect(t, old_pid, d.process.GetPid())

	// kill for test once
	d.Control.fifo <- Return{err: nil, msg: "k"}
	// process shuld not start and pids remains the same
	expect(t, d.count, uint32(1))
	expect(t, d.count_defer, uint32(1))
	expect(t, old_pid, d.process.GetPid())

	for sup.IsRunning(d.process.GetPid()) {
		time.Sleep(time.Second)
	}

	//// bring up the service (new pid expected)
	//d.Control.fifo <- Return{err: nil, msg: "u"}
	//for old_pid == d.process.GetPid() {
	//// waiting to change pid
	//}
	//expect(t, d.count, uint32(1))
	//expect(t, d.count_defer, uint32(0))

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

	// test u
	// bring up the service (new pid expected)
	d.Control.fifo <- Return{err: nil, msg: "u"}
	for old_pid == d.process.GetPid() {
		time.Sleep(time.Second)
	}
	expect(t, d.count, uint32(1))
	expect(t, d.count_defer, uint32(0))

	// create zombie
	d.Control.fifo <- Return{err: nil, msg: "d"}
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
