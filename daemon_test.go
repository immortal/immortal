package immortal

import (
	"io/ioutil"
	"os"
	"os/signal"
	"path/filepath"
	"sync/atomic"
	"testing"
	"time"
)

func TestDaemonNewCtrl(t *testing.T) {
	dir, err := ioutil.TempDir("", "TestDaemonNewCtrl")
	if err != nil {
		t.Error(err)
	}
	defer os.RemoveAll(dir)
	cfg := &Config{
		Cwd:  dir,
		ctrl: true,
	}
	d, err := New(cfg)
	if err != nil {
		t.Error(err)
	}
	f, err := os.Stat(filepath.Join(dir, "supervise/control"))
	if f.Mode()&os.ModeType != os.ModeNamedPipe {
		t.Error("Expecting os.ModeNamePipe")
	}
	f, err = os.Stat(filepath.Join(dir, "supervise/ok"))
	if f.Mode()&os.ModeType != os.ModeNamedPipe {
		t.Error("Expecting os.ModeNamePipe")
	}
	if _, err = os.Stat(filepath.Join(dir, "supervise/lock")); err != nil {
		t.Error(err)
	}
	expect(t, uint32(0), d.lock)
	expect(t, uint32(0), d.lock_once)
	// test lock
	_, err = New(cfg)
	if err == nil {
		t.Error("Expecting error: resource temporarily unavailable")
	}
}

func TestDaemonNewCtrlErr(t *testing.T) {
	dir, err := ioutil.TempDir("", "TestDaemonNewCtrlErr")
	if err != nil {
		t.Error(err)
	}
	defer os.RemoveAll(dir)
	os.Chmod(dir, 0000)
	cfg := &Config{
		Cwd:  dir,
		ctrl: true,
	}
	_, err = New(cfg)
	if err == nil {
		t.Error("Expecting error")
	}
}

func TestDaemonNewCtrlCwd(t *testing.T) {
	dir, err := ioutil.TempDir("", "TestDaemonNewCtrlCwd")
	if err != nil {
		t.Error(err)
	}
	defer os.RemoveAll(dir)
	cwd, err := os.Getwd()
	if err != nil {
		t.Error(err)
	}
	defer os.Chdir(cwd)
	if err := os.Chdir(dir); err != nil {
		t.Error(err)
	}
	cfg := &Config{
		ctrl: true,
	}
	d, err := New(cfg)
	if err != nil {
		t.Error(err)
	}
	f, err := os.Stat(filepath.Join(dir, "supervise/control"))
	if f.Mode()&os.ModeType != os.ModeNamedPipe {
		t.Error("Expecting os.ModeNamePipe")
	}
	f, err = os.Stat(filepath.Join(dir, "supervise/ok"))
	if f.Mode()&os.ModeType != os.ModeNamedPipe {
		t.Error("Expecting os.ModeNamePipe")
	}
	if _, err = os.Stat(filepath.Join(dir, "supervise/lock")); err != nil {
		t.Error(err)
	}
	expect(t, uint32(0), d.lock)
	expect(t, uint32(0), d.lock_once)
	// test lock
	_, err = New(cfg)
	if err == nil {
		t.Error("Expecting error: resource temporarily unavailable")
	}
}

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
		command: []string{filepath.Join(dirBase, base), "-test.run=TestHelperProcessSignalsUDOT", "--"},
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

	p, err := d.Run(NewProcess(cfg))
	if err != nil {
		t.Error(err)
	}

	sup := &Sup{p}

	// check pids
	if pid, err := sup.ReadPidFile(filepath.Join(parentDir, "parent.pid")); err != nil {
		t.Error(err)
	} else {
		expect(t, os.Getpid(), pid)
	}
	if pid, err := sup.ReadPidFile(filepath.Join(parentDir, "child.pid")); err != nil {
		t.Error(err, pid)
	} else {
		expect(t, p.Pid(), pid)
	}

	// test "k", process should restart and get a new pid
	t.Log("testing k")
	current_pid := p.Pid()
	sup.HandleSignals("k", d)
	// wait for process to finish
	err = <-p.errch
	atomic.StoreUint32(&d.lock, d.lock_once)
	expect(t, "signal: killed", err.Error())
	p, err = d.Run(NewProcess(cfg))
	if err != nil {
		t.Error(err)
	}
	sup = &Sup{p}
	if current_pid == p.Pid() {
		t.Fatalf("Expecting a new pid")
	}

	// test "d", (keep it down and don't restart)
	t.Log("testing d")
	sup.HandleSignals("d", d)
	// wait for process to finish
	err = <-p.errch
	atomic.StoreUint32(&d.lock, d.lock_once)
	expect(t, "signal: terminated", err.Error())
	p, err = d.Run(NewProcess(cfg))
	if err == nil {
		t.Error("Expecting an error")
	}

	// test "u"
	t.Log("testing up")
	sup.HandleSignals("u", d)
	p, err = d.Run(NewProcess(cfg))
	if err != nil {
		t.Error(err)
	}
	sup = &Sup{p}

	// test "once", process should not restart after going down
	t.Log("testing once")
	sup.HandleSignals("o", d)
	sup.HandleSignals("k", d)
	// wait for process to finish
	err = <-p.errch
	atomic.StoreUint32(&d.lock, d.lock_once)
	expect(t, "signal: killed", err.Error())
	p, err = d.Run(NewProcess(cfg))
	if err == nil {
		t.Error("Expecting an error")
	}
	sup = &Sup{p}

	// test "u"
	t.Log("testing u")
	sup.HandleSignals("u", d)
	p, err = d.Run(NewProcess(cfg))
	if err != nil {
		t.Error(err)
	}
	sup = &Sup{p}
	old_pid := p.Pid()

	// test "t"
	t.Log("testing t")
	sup.HandleSignals("t", d)
	err = <-p.errch
	atomic.StoreUint32(&d.lock, d.lock_once)
	expect(t, "signal: terminated", err.Error())
	// restart to get new pid
	p, err = d.Run(NewProcess(cfg))
	if err != nil {
		t.Error(err)
	}
	sup = &Sup{p}
	if old_pid == p.Pid() {
		t.Fatal("Expecting a new pid")
	}
	sup.HandleSignals("kill", d)
	err = <-p.errch
	atomic.StoreUint32(&d.lock, d.lock_once)
	expect(t, "signal: killed", err.Error())
}
