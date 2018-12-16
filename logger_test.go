package immortal

import (
	"io/ioutil"
	"log"
	"os"
	"path/filepath"
	"strings"
	"sync"
	"testing"
	"time"
)

func TestNewStderrLogger(t *testing.T) {
	tmpfile, err := ioutil.TempFile("", "stderr")
	if err != nil {
		log.Fatal(err)
	}
	defer os.Remove(tmpfile.Name())
	log.SetOutput(ioutil.Discard)
	cfg := &Config{
		Stderr: Log{
			File: tmpfile.Name(),
		},
	}
	l := NewStderrLogger(cfg)
	expect(t, true, l != nil)
}

func TestNewStderrLoggerFileNone(t *testing.T) {
	log.SetOutput(ioutil.Discard)
	cfg := &Config{
		Stderr: Log{
			File: "/dev/null/nonexist",
		},
	}
	l := NewStderrLogger(cfg)
	expect(t, true, l == nil)
}

func TestNewLoggerFileNone(t *testing.T) {
	log.SetOutput(ioutil.Discard)
	cfg := &Config{
		Log: Log{
			File: "/dev/null/nonexist",
		},
	}
	quit := make(chan struct{})
	l := NewLogger(cfg, quit)
	expect(t, true, l == nil)
}

func TestNewLoggerBadLogger(t *testing.T) {
	log.SetOutput(ioutil.Discard)
	cfg := &Config{
		Logger: "any-logger",
	}
	quit := make(chan struct{})
	l := NewLogger(cfg, quit)
	expect(t, true, l == nil)
}

func TestNewLoggerLogger(t *testing.T) {
	cfg := &Config{
		Logger: "cat",
	}
	quit := make(chan struct{})
	l := NewLogger(cfg, quit)
	expect(t, true, l != nil)
	close(quit)
}

func TestNewLoggerKilled(t *testing.T) {
	cfg := &Config{
		Logger: "sleep 300",
	}
	quit := make(chan struct{})
	l := NewLogger(cfg, quit)
	expect(t, true, l != nil)
	var wg sync.WaitGroup
	wg.Add(1)
	go func() {
		time.Sleep(time.Second)
		close(quit)
		wg.Done()
	}()
	wg.Wait()
	// TODO verify sleep process is killed
}

func TestNewLoggerRetry(t *testing.T) {
	var mylog myBuffer
	log.SetOutput(&mylog)
	log.SetFlags(0)
	cfg := &Config{
		Logger: "false",
	}
	quit := make(chan struct{})
	l := NewLogger(cfg, quit)
	expect(t, true, l != nil)
	var wg sync.WaitGroup
	wg.Add(1)
	go func() {
		time.Sleep(2 * time.Second)
		close(quit)
		wg.Done()
	}()
	wg.Wait()
	m := strings.Split(mylog.String(), "\n")
	expect(t, true, len(m) > 1)
}

func TestLogWriterLog(t *testing.T) {
	sdir, err := ioutil.TempDir("", "TestLogWriterLog")
	if err != nil {
		t.Error(err)
	}
	defer os.RemoveAll(sdir)
	tmpfile, err := ioutil.TempFile(sdir, "log.")
	if err != nil {
		t.Error(err)
	}
	cfg := &Config{
		Env:     map[string]string{"GO_WANT_HELPER_PROCESS": "logSIGPIPE"},
		command: []string{os.Args[0]},
		Cwd:     sdir,
		ctl:     sdir,
		Pid: Pid{
			Parent: filepath.Join(sdir, "parent.pid"),
			Child:  filepath.Join(sdir, "child.pid"),
		},
		Log: Log{
			File: tmpfile.Name(),
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

	select {
	case err = <-p.errch:
		t.Fatal(err)
		break
	case <-time.After(2 * time.Second):
		break
	}

	// closing socket
	close(d.quit)
	d.wg.Wait()
}
