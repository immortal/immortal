package immortal

import (
	"io/ioutil"
	"os"
	"os/signal"
	"path/filepath"
	"testing"
	"time"
)

func TestReadPidFileNonexistent(t *testing.T) {
	sup := new(Sup)
	i, e := sup.ReadPidFile("nonexistent")
	if i != 0 {
		t.Errorf("Expecting: 0 got: %v", i)
	}
	if e == nil {
		t.Errorf("Expecting: no such file or directory")
	}
}

func TestReadPidFileBadContent(t *testing.T) {
	sup := new(Sup)
	i, e := sup.ReadPidFile("funcs.go")
	if i != 0 {
		t.Errorf("Expecting: 0 got: %v", i)
	}
	if e == nil {
		t.Errorf("Expecting: no such file or directory")
	}
}

func TestReadPidFile(t *testing.T) {
	content := []byte("1234")
	tmpfile, err := ioutil.TempFile("", "TestReadPidfile")
	if err != nil {
		t.Error(err)
	}

	defer os.Remove(tmpfile.Name()) // clean up

	if _, err := tmpfile.Write(content); err != nil {
		t.Error(err)
	}
	if err := tmpfile.Close(); err != nil {
		t.Error(err)
	}
	sup := new(Sup)
	i, e := sup.ReadPidFile(tmpfile.Name())
	if i != 1234 {
		t.Errorf("Expecting: 1234 got: %v", i)
	}
	if e != nil {
		t.Error(e)
	}
}

func TestHelperProcessSup(*testing.T) {
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

func TestSupervise(t *testing.T) {
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
		command: []string{filepath.Join(dirBase, base), "-test.run=TestHelperProcessSup"},
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
	go Supervise(sup, d)

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

	select {
	case <-time.After(1 * time.Second):
		d.Control.fifo <- Return{err: nil, msg: "x"}
	}
}
