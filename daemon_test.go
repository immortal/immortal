package immortal

import (
	"bytes"
	"fmt"
	"io/ioutil"
	"log"
	"os"
	"os/signal"
	"os/user"
	"path/filepath"
	"strings"
	"sync/atomic"
	"testing"
	"time"
)

func TestDaemonNewCtl(t *testing.T) {
	dir, err := ioutil.TempDir("", "TestDaemonNewCtl")
	if err != nil {
		t.Error(err)
	}
	defer os.RemoveAll(dir)
	cfg := &Config{
		Cwd: dir,
		ctl: dir,
	}
	d, err := New(cfg)
	if err != nil {
		t.Fatal(err)
	}
	if _, err = os.Stat(filepath.Join(dir, "lock")); err != nil {
		t.Fatal(err)
	}
	expect(t, uint32(0), d.lock)
	expect(t, uint32(0), d.lockOnce)
	// test lock
	_, err = New(cfg)
	if err == nil {
		t.Error("Expecting error: resource temporarily unavailable")
	}
}

func TestDaemonNewCtlErr(t *testing.T) {
	dir, err := ioutil.TempDir("", "TestDaemonNewCtlErr")
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
	os.Chmod(dir, 0000)
	cfg := &Config{
		ctl: dir,
	}
	_, err = New(cfg)
	if err == nil {
		t.Error("Expecting error")
	}
}

func TestDaemonNewCtlCwd(t *testing.T) {
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
		ctl: dir,
	}
	d, err := New(cfg)
	if err != nil {
		t.Error(err)
	}
	if _, err = os.Stat(filepath.Join(dir, "lock")); err != nil {
		t.Fatal(err)
	}
	expect(t, uint32(0), d.lock)
	expect(t, uint32(0), d.lockOnce)
	// test lock
	_, err = New(cfg)
	if err == nil {
		t.Error("Expecting error: resource temporarily unavailable")
	}
}

func TestBadUid(t *testing.T) {
	dir, err := ioutil.TempDir("", "TestBadUid")
	if err != nil {
		t.Error(err)
	}
	defer os.RemoveAll(dir)
	cfg := &Config{
		command: []string{"go"},
		user:    &user.User{Uid: "uid", Gid: "0"},
		ctl:     dir,
	}
	d, err := New(cfg)
	if err != nil {
		t.Fatal(err)
	}
	_, err = d.Run(NewProcess(cfg))
	if err == nil {
		t.Error("Expecting error")
	}
}

func TestBadGid(t *testing.T) {
	dir, err := ioutil.TempDir("", "TestBadGid")
	if err != nil {
		t.Error(err)
	}
	defer os.RemoveAll(dir)
	cfg := &Config{
		command: []string{"go"},
		user:    &user.User{Uid: "0", Gid: "gid"},
		ctl:     dir,
	}
	d, err := New(cfg)
	if err != nil {
		t.Fatal(err)
	}
	_, err = d.Run(NewProcess(cfg))
	if err == nil {
		t.Error("Expecting error")
	}
}

func TestUser(t *testing.T) {
	dir, err := ioutil.TempDir("", "TestUser")
	if err != nil {
		t.Error(err)
	}
	defer os.RemoveAll(dir)
	cfg := &Config{
		command: []string{"go"},
		user:    &user.User{Uid: "0", Gid: "0"},
		ctl:     dir,
	}
	d, err := New(cfg)
	if err != nil {
		t.Fatal(err)
	}
	_, err = d.Run(NewProcess(cfg))
	if err == nil {
		t.Error("Expecting error")
	}
}

func TestBadWritePidParent(t *testing.T) {
	dir, err := ioutil.TempDir("", "TestBadWritePidParent")
	if err != nil {
		t.Error(err)
	}
	defer os.RemoveAll(dir)
	var mylog bytes.Buffer
	log.SetOutput(&mylog)
	log.SetFlags(0)
	cfg := &Config{
		command: []string{"go"},
		Pid: Pid{
			Parent: "/dev/null/parent.pid",
		},
		ctl: dir,
	}
	d, err := New(cfg)
	if err != nil {
		t.Fatal(err)
	}
	_, err = d.Run(NewProcess(cfg))
	if err != nil {
		t.Fatal(err)
	}
	expect(t, "open /dev/null/parent.pid: not a directory", strings.TrimSpace(mylog.String()))
}

func TestBadWritePidChild(t *testing.T) {
	var mylog bytes.Buffer
	log.SetOutput(&mylog)
	log.SetFlags(0)
	cfg := &Config{
		command: []string{"go"},
		Pid: Pid{
			Child: "/dev/null/child.pid",
		},
	}
	d, err := New(cfg)
	if err != nil {
		t.Fatal(err)
	}
	_, err = d.Run(NewProcess(cfg))
	if err != nil {
		t.Fatal(err)
	}
	expect(t, "open /dev/null/child.pid: not a directory", strings.TrimSpace(mylog.String()))
}

func TestHelperProcessSignalsUDOT(*testing.T) {
	if os.Getenv("GO_WANT_HELPER_PROCESS") != "1" {
		return
	}
	fmt.Println("5D675098-45D7-4089-A72C-3628713EA5BA")
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
	sdir, err := ioutil.TempDir("", "TestSignalsUDOT")
	if err != nil {
		t.Error(err)
	}
	defer os.RemoveAll(sdir)
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
	tmpfile, err := ioutil.TempFile("", "TestLogFile")
	if err != nil {
		t.Error(err)
	}
	defer os.Remove(tmpfile.Name()) // clean up
	cfg := &Config{
		Env:     map[string]string{"GO_WANT_HELPER_PROCESS": "1"},
		command: []string{filepath.Join(dirBase, base), "-test.run=TestHelperProcessSignalsUDOT", "--"},
		Cwd:     parentDir,
		Pid: Pid{
			Parent: filepath.Join(parentDir, "parent.pid"),
			Child:  filepath.Join(parentDir, "child.pid"),
		},
		Log: Log{
			File: tmpfile.Name(),
		},
		ctl: sdir,
	}
	d, err := New(cfg)
	if err != nil {
		t.Fatal(err)
	}

	np := NewProcess(cfg)
	expect(t, 0, np.Pid())
	p, err := d.Run(np)
	if err != nil {
		t.Error(err)
	}

	// create socket
	if err := d.Listen(); err != nil {
		t.Fatal(err)
	}

	// check pids
	if pid, err := d.ReadPidFile(filepath.Join(parentDir, "parent.pid")); err != nil {
		t.Error(err)
	} else {
		expect(t, os.Getpid(), pid)
	}
	if pid, err := d.ReadPidFile(filepath.Join(parentDir, "child.pid")); err != nil {
		t.Error(err, pid)
	} else {
		expect(t, p.Pid(), pid)
	}

	// check lock
	if _, err = os.Stat(filepath.Join(sdir, "immortal.sock")); err != nil {
		t.Fatal(err)
	}

	status := &Status{}
	ctl := &Controller{}
	signalResponse := &SignalResponse{}
	if status, err = ctl.GetStatus(filepath.Join(sdir, "immortal.sock")); err != nil {
		t.Fatal(err)
	}
	expect(t, true, strings.HasSuffix(status.Cmd, "/immortal.test -test.run=TestHelperProcessSignalsUDOT --"))
	expect(t, 1, int(status.Count))

	// http socket client
	// test "k", process should restart and get a new pid
	t.Log("testing k")
	expect(t, p.Pid(), status.Pid)

	if signalResponse, err = ctl.SendSignal(filepath.Join(sdir, "immortal.sock"), "k"); err != nil {
		t.Fatal(err)
	}
	expect(t, "", signalResponse.Err)

	// wait for process to finish
	err = <-p.errch
	atomic.StoreUint32(&d.lock, d.lockOnce)
	expect(t, "signal: killed", err.Error())
	p, err = d.Run(NewProcess(cfg))
	if err != nil {
		t.Error(err)
	}

	if status.Pid == p.Pid() {
		t.Fatalf("Expecting a new pid")
	}

	// $ pgrep -fl TestHelperProcessSignalsUDO
	// PID _test/immortal.test -test.run=TestHelperProcessSignalsUDOT --

	// test "d", (keep it down and don't restart)
	t.Log("testing d")
	if _, err := ctl.SendSignal(filepath.Join(sdir, "immortal.sock"), "d"); err != nil {
		t.Fatal(err)
	}
	// wait for process to finish
	err = <-p.errch
	atomic.StoreUint32(&d.lock, d.lockOnce)
	expect(t, "signal: terminated", err.Error())
	np = NewProcess(cfg)
	p, err = d.Run(np)
	if err == nil {
		t.Error("Expecting an error")
	} else {
		close(np.quit)
	}

	// test "u"
	t.Log("testing up")
	go func() {
		if _, err := ctl.SendSignal(filepath.Join(sdir, "immortal.sock"), "up"); err != nil {
			t.Fatal(err)
		}
	}()
	<-d.run
	p, err = d.Run(NewProcess(cfg))
	if err != nil {
		t.Error(err)
	}

	// test "once", process should not restart after going down
	t.Log("testing once")
	if _, err := ctl.SendSignal(filepath.Join(sdir, "immortal.sock"), "o"); err != nil {
		t.Fatal(err)
	}

	if _, err := ctl.SendSignal(filepath.Join(sdir, "immortal.sock"), "k"); err != nil {
		t.Fatal(err)
	}
	// wait for process to finish
	err = <-p.errch
	atomic.StoreUint32(&d.lock, d.lockOnce)
	expect(t, "signal: killed", err.Error())
	np = NewProcess(cfg)
	p, err = d.Run(np)
	if err == nil {
		t.Error("Expecting an error")
	} else {
		close(np.quit)
	}

	if status, err = ctl.GetStatus(filepath.Join(sdir, "immortal.sock")); err != nil {
		t.Fatal(err)
	}
	expect(t, 3, int(status.Count))

	// test "u"
	t.Log("testing u")
	go func() {
		if _, err := ctl.SendSignal(filepath.Join(sdir, "immortal.sock"), "u"); err != nil {
			t.Fatal(err)
		}
	}()
	<-d.run
	p, err = d.Run(NewProcess(cfg))
	if err != nil {
		t.Error(err)
	}
	oldPid := p.Pid()

	// test "t"
	t.Log("testing t")
	if _, err := ctl.SendSignal(filepath.Join(sdir, "immortal.sock"), "t"); err != nil {
		t.Fatal(err)
	}
	err = <-p.errch
	atomic.StoreUint32(&d.lock, d.lockOnce)
	expect(t, "signal: terminated", err.Error())

	// restart to get new pid
	p, err = d.Run(NewProcess(cfg))
	if err != nil {
		t.Error(err)
	}
	if oldPid == p.Pid() {
		t.Fatal("Expecting a new pid")
	}
	if _, err := ctl.SendSignal(filepath.Join(sdir, "immortal.sock"), "kill"); err != nil {
		t.Fatal(err)
	}
	err = <-p.errch
	atomic.StoreUint32(&d.lock, d.lockOnce)
	expect(t, "signal: killed", err.Error())

	// test after
	p, err = d.Run(NewProcess(cfg))
	if err != nil {
		t.Error(err)
	}

	select {
	case err := <-p.errch:
		expect(t, "signal: killed", err.Error())
	case <-time.After(1 * time.Second):
		if _, err := ctl.SendSignal(filepath.Join(sdir, "immortal.sock"), "kill"); err != nil {
			t.Fatal(err)
		}
	}

	if status, err = ctl.GetStatus(filepath.Join(sdir, "immortal.sock")); err != nil {
		t.Fatal(err)
	}
	expect(t, 6, int(status.Count))

	// test log content
	t.Log("testing logfile")
	content, err := ioutil.ReadFile(tmpfile.Name())
	if err != nil {
		t.Fatal(err)
	}
	lines := strings.Split(string(content), "\n")
	expect(t, true, strings.HasSuffix(lines[0], "5D675098-45D7-4089-A72C-3628713EA5BA"))
}
