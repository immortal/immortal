package immortal

import (
	"io/ioutil"
	"log"
	"os"
	"os/signal"
	"path/filepath"
	"strconv"
	"strings"
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
	expect(t, uint32(0), d.lock_defer)
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
	expect(t, uint32(0), d.lock_defer)
	// test lock
	_, err = New(cfg)
	if err == nil {
		t.Error("Expecting error: resource temporarily unavailable")
	}
}

func TestWritePid(t *testing.T) {
	cfg := &Config{}
	d, err := New(cfg)
	if err != nil {
		t.Error(err)
	}
	tmpfile, err := ioutil.TempFile("", "TestWritePid")
	if err != nil {
		t.Error(err)
	}
	defer os.Remove(tmpfile.Name()) // clean up
	err = d.WritePid(tmpfile.Name(), 1234)
	if err != nil {
		t.Error(err)
	}
	content, err := ioutil.ReadFile(tmpfile.Name())
	if err != nil {
		t.Error(err)
	}
	lines := strings.Split(string(content), "\n")
	pid, err := strconv.Atoi(lines[0])
	if err != nil {
		t.Error(err)
	}
	expect(t, pid, 1234)
}

func TestWritePidErr(t *testing.T) {
	cfg := &Config{}
	d, err := New(cfg)
	if err != nil {
		t.Error(err)
	}
	tmpfile, err := ioutil.TempFile("", "TestWritePid")
	if err != nil {
		t.Error(err)
	}
	defer os.Remove(tmpfile.Name()) // clean up
	os.Chmod(tmpfile.Name(), 0444)
	err = d.WritePid(tmpfile.Name(), 1234)
	if err == nil {
		t.Error("Expecting error: permission denied")
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
	d.Run()
	sup := &Sup{time.Now()}

	// check pids
	if pid, err := sup.ReadPidFile(filepath.Join(parentDir, "parent.pid")); err != nil {
		t.Error(err)
	} else {
		expect(t, os.Getpid(), pid)
	}
	if pid, err := sup.ReadPidFile(filepath.Join(parentDir, "child.pid")); err != nil {
		t.Error(err)
	} else {
		expect(t, d.Process().Pid, pid)
	}

	old_pid := d.Process().Pid
	for d.start.IsZero() {
		// wait process to start
	}
	// test "k", process should restart and get a new pid
	sup.HandleSignals("k", d)
	expect(t, uint32(1), d.lock)
	expect(t, uint32(0), d.lock_defer)
	done := make(chan struct{}, 1)
	select {
	case <-d.Control.state:
		d.cmd.Process.Pid = 0
		d.start = time.Date(1, 1, 1, 0, 0, 0, 0, time.UTC)
		done <- struct{}{}
	}
	select {
	case <-done:
		d.Run()
	}

	if old_pid == d.Process().Pid {
		t.Fatal("Expecting a new pid")
	}

	for d.start.IsZero() {
		// wait process to start
	}

	// test "d", (keep it down and don't restart)
	sup.HandleSignals("d", d)
	select {
	case <-d.Control.state:
		d.start = time.Date(1, 1, 1, 0, 0, 0, 0, time.UTC)
		d.cmd.Process.Pid = 0
		done <- struct{}{}
	}
	select {
	case <-done:
		d.Run()
	}
	expect(t, 0, d.Process().Pid)

	// test "u" more debug with: watch -n 0.1 "pgrep -fl run=TestSignals | awk '{print $1}' | xargs -n1 pstree -p "
	sup.HandleSignals("u", d)

	for d.start.IsZero() {
		// wait process to start
	}

	// test "once", process should not restart after going down
	sup.HandleSignals("o", d)
	sup.HandleSignals("k", d)
	select {
	case <-d.Control.state:
		d.start = time.Date(1, 1, 1, 0, 0, 0, 0, time.UTC)
		d.cmd.Process.Pid = 0
		done <- struct{}{}
	}
	select {
	case <-done:
		d.Run()
	}
	expect(t, 0, d.Process().Pid)

	// test "up"
	sup.HandleSignals("up", d)
	sup.HandleSignals("stop", d)
	sup.HandleSignals("cont", d)
	sup.HandleSignals("t", d)
	select {
	case <-d.Control.state:
		d.start = time.Date(1, 1, 1, 0, 0, 0, 0, time.UTC)
		d.cmd.Process.Pid = 0
		done <- struct{}{}
	}
	select {
	case <-done:
		d.Run()
	}
	// after exiting will get a race cond
	expect(t, true, d.Process().Pid > 0)
	sup.HandleSignals("exit", d)
}
