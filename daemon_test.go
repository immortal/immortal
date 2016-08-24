package immortal

import (
	"fmt"
	"io/ioutil"
	"os"
	"os/signal"
	"path/filepath"
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

	go d.Run(NewProcess(cfg))

	fmt.Printf("d.pong() = %+v\n", d.pong())
	for {
		fmt.Println(d.pong(), 333)
		time.Sleep(2 * time.Second)
	}

	sup := &Sup{}

	// check pids
	if pid, err := sup.ReadPidFile(filepath.Join(parentDir, "parent.pid")); err != nil {
		t.Error(err)
	} else {
		expect(t, os.Getpid(), pid)
	}
	if pid, err := sup.ReadPidFile(filepath.Join(parentDir, "child.pid")); err != nil {
		t.Error(err, pid)
	} else {
		expect(t, d.process.Pid(), pid)
	}
	old_pid := d.process.Pid()

	for {
		fmt.Println("waiting...", old_pid)
		time.Sleep(1 * time.Second)
	}

	/*
		// test "k", process should restart and get a new pid
		sup.HandleSignals("k", d)
		t.Log("testing k")

		// wait for process to finish
		<-d.Control.done

		d.Run()
		<-d.running

		if old_pid == d.Process.Pid() {
			t.Fatal("Expecting a new pid")
		}
		old_pid = d.Process.Pid()

		t.Log("testing d")
		// test "d", (keep it down and don't restart)
		sup.HandleSignals("d", d)

		// wait for process to finish
		<-d.Control.done

		d.Run()
		d.Run()
		d.Run()
		<-d.running
		t.Log("testing up")
		sup.HandleSignals("u", d)

		d.Run()
		<-d.running

		if old_pid == d.Process.Pid() {
			t.Fatal("Expecting a new pid")
		}
		old_pid = d.Process.Pid()

		t.Log("testing once")
		// test "once", process should not restart after going down
		sup.HandleSignals("o", d)
		sup.HandleSignals("k", d)

		// wait for process to finish
		<-d.Control.done

		d.Run()
		d.Run()
		d.Run()
		<-d.running
		if old_pid != d.Process.Pid() {
			t.Fatal("Expecting same pid, process should not restart")
		}

		sup.HandleSignals("u", d)

		d.Run()
		<-d.running

		if old_pid == d.Process.Pid() {
			t.Fatal("Expecting a new pid")
		}
		old_pid = d.Process.Pid()

		t.Log("testing t")
		sup.HandleSignals("t", d)

		// wait for process to finish
		<-d.Control.done

		d.Run()
		<-d.running

		if old_pid == d.Process.Pid() {
			t.Fatal("Expecting a new pid")
		}

		sup.HandleSignals("k", d)
		sup.HandleSignals("exit", d)
	*/
}
