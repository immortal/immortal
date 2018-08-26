package immortal

import (
	"fmt"
	"io/ioutil"
	"os"
	"path/filepath"
	"testing"
)

func TestProcessStart(t *testing.T) {
	cfg := &Config{
		command: []string{"--"},
	}

	quit := make(chan struct{})
	np := &process{
		Config: cfg,
		Logger: &LogWriter{
			logger: NewLogger(cfg, quit),
		},
		LoggerStderr: &LogWriter{
			logger: NewStderrLogger(cfg),
		},
		quit: quit,
	}
	_, err := np.Start()
	if err == nil {
		t.Error("Expecting exec: --: executable file not found in $PATH")
	}
}

func TestProcessLogStderrStdout(t *testing.T) {
	sdir, err := ioutil.TempDir("", "TestProcessLogStderrStdout")
	if err != nil {
		t.Error(err)
	}
	defer os.RemoveAll(sdir)
	tmpfileStdout, err := ioutil.TempFile(sdir, "log.stdout")
	if err != nil {
		t.Error(err)
	}
	tmpfileStderr, err := ioutil.TempFile(sdir, "log.stderr")
	if err != nil {
		t.Error(err)
	}
	cfg := &Config{
		Env:     map[string]string{"GO_WANT_HELPER_PROCESS": "logStdoutStderr"},
		command: []string{os.Args[0]},
		Cwd:     sdir,
		ctl:     sdir,
		Pid: Pid{
			Parent: filepath.Join(sdir, "parent.pid"),
			Child:  filepath.Join(sdir, "child.pid"),
		},
		Log: Log{
			File: tmpfileStdout.Name(),
		},
		Stderr: Log{
			File: tmpfileStderr.Name(),
		},
	}

	// create new daemon
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
	if pid, err := d.ReadPidFile(filepath.Join(sdir, "parent.pid")); err != nil {
		t.Error(err)
	} else {
		expect(t, os.Getpid(), pid)
	}
	if pid, err := d.ReadPidFile(filepath.Join(sdir, "child.pid")); err != nil {
		t.Error(err, pid)
	} else {
		expect(t, p.Pid(), pid)
	}

	// check lock
	if _, err = os.Stat(filepath.Join(sdir, "immortal.sock")); err != nil {
		t.Fatal(err)
	}

	// wait for process to finish
	err = <-p.errch

	t.Log("verifying stdout")
	content, err := ioutil.ReadFile(tmpfileStdout.Name())
	if err != nil {
		t.Fatal(err)
	}
	expectStdout := fmt.Sprintf("STDOUT i: 1\nSTDOUT i: 2\nSTDOUT i: 4\n")
	expect(t, expectStdout, string(content))

	t.Log("verifying stderr")
	content, err = ioutil.ReadFile(tmpfileStderr.Name())
	if err != nil {
		t.Fatal(err)
	}
	expectStderr := fmt.Sprintf("STDERR i: 3\n")
	expect(t, expectStderr, string(content))

	// closing socket
	close(d.quit)
	d.wg.Wait()
}

func TestProcessLogStderr(t *testing.T) {
	sdir, err := ioutil.TempDir("", "TestProcessLogStderr")
	if err != nil {
		t.Error(err)
	}
	defer os.RemoveAll(sdir)
	tmpfileStderr, err := ioutil.TempFile(sdir, "log.stderr")
	if err != nil {
		t.Error(err)
	}
	cfg := &Config{
		Env:     map[string]string{"GO_WANT_HELPER_PROCESS": "logStdoutStderr"},
		command: []string{os.Args[0]},
		Cwd:     sdir,
		ctl:     sdir,
		Pid: Pid{
			Parent: filepath.Join(sdir, "parent.pid"),
			Child:  filepath.Join(sdir, "child.pid"),
		},
		Stderr: Log{
			File: tmpfileStderr.Name(),
		},
	}

	// create new daemon
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
	if pid, err := d.ReadPidFile(filepath.Join(sdir, "parent.pid")); err != nil {
		t.Error(err)
	} else {
		expect(t, os.Getpid(), pid)
	}
	if pid, err := d.ReadPidFile(filepath.Join(sdir, "child.pid")); err != nil {
		t.Error(err, pid)
	} else {
		expect(t, p.Pid(), pid)
	}

	// check lock
	if _, err = os.Stat(filepath.Join(sdir, "immortal.sock")); err != nil {
		t.Fatal(err)
	}

	// wait for process to finish
	err = <-p.errch

	t.Log("verifying stderr")
	content, err := ioutil.ReadFile(tmpfileStderr.Name())
	if err != nil {
		t.Fatal(err)
	}
	expectStderr := fmt.Sprintf("STDERR i: 3\n")
	expect(t, expectStderr, string(content))

	// closing socket
	close(d.quit)
	d.wg.Wait()
}
