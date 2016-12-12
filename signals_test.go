package immortal

import (
	"bytes"
	"fmt"
	"io/ioutil"
	"log"
	"os"
	"os/signal"
	"path/filepath"
	"strings"
	"sync/atomic"
	"syscall"
	"testing"
	"time"
)

func TestHelperProcessSignalsFiFo(*testing.T) {
	if os.Getenv("GO_WANT_HELPER_PROCESS") != "1" {
		return
	}
	tmpfile := os.Getenv("TEST_SIGNALS_FILE")
	c := make(chan os.Signal, 1)
	signal.Notify(c,
		syscall.SIGALRM,
		syscall.SIGCONT,
		syscall.SIGHUP,
		syscall.SIGINT,
		syscall.SIGQUIT,
		syscall.SIGTTIN,
		syscall.SIGTTOU,
		syscall.SIGUSR1,
		syscall.SIGUSR2,
		syscall.SIGWINCH,
	)
	for {
		signalType := <-c
		switch signalType {
		case syscall.SIGALRM:
			ioutil.WriteFile(tmpfile, []byte("--a"), 0644)
		case syscall.SIGCONT:
			ioutil.WriteFile(tmpfile, []byte("--c"), 0644)
		case syscall.SIGHUP:
			ioutil.WriteFile(tmpfile, []byte("--h"), 0644)
		case syscall.SIGINT:
			ioutil.WriteFile(tmpfile, []byte("--i"), 0644)
		case syscall.SIGQUIT:
			ioutil.WriteFile(tmpfile, []byte("--q"), 0644)
		case syscall.SIGTTIN:
			ioutil.WriteFile(tmpfile, []byte("--in"), 0644)
		case syscall.SIGTTOU:
			ioutil.WriteFile(tmpfile, []byte("--ou"), 0644)
		case syscall.SIGUSR1:
			ioutil.WriteFile(tmpfile, []byte("--1"), 0644)
		case syscall.SIGUSR2:
			ioutil.WriteFile(tmpfile, []byte("--2"), 0644)
		case syscall.SIGWINCH:
			ioutil.WriteFile(tmpfile, []byte("--w"), 0644)
		}
	}
}

func TestSignalsFiFo(t *testing.T) {
	var mylog bytes.Buffer
	log.SetOutput(&mylog)
	log.SetFlags(0)
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
	// for writing the signals
	tmpfile, err := ioutil.TempFile("", "signals")
	if err != nil {
		t.Fatal(err)
	}
	defer os.Remove(tmpfile.Name()) // clean up

	cfg := &Config{
		Env:     map[string]string{"GO_WANT_HELPER_PROCESS": "1", "TEST_SIGNALS_FILE": tmpfile.Name()},
		command: []string{filepath.Join(dirBase, base), "-test.run=TestHelperProcessSignalsFiFo", "--"},
		Cwd:     parentDir,
		Pid: Pid{
			Parent: filepath.Join(parentDir, "parent.pid"),
			Child:  filepath.Join(parentDir, "child.pid"),
		},
		ctl: true,
	}
	d, err := New(cfg)
	if err != nil {
		t.Error(err)
	}

	p, err := d.Run(NewProcess(cfg))
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
		t.Error(err)
	} else {
		expect(t, p.Pid(), pid)
	}

	// sync
	time.Sleep(time.Second)

	var testSignals = []struct {
		signal   string
		expected string
	}{
		{"a", "--a"},
		{"alrm", "--a"},
		{"c", "--c"},
		{"cont", "--c"},
		{"h", "--h"},
		{"hup", "--h"},
		{"i", "--i"},
		{"int", "--i"},
		{"q", "--q"},
		{"quit", "--q"},
		{"in", "--in"},
		{"TTIN", "--in"},
		{"ou", "--ou"},
		{"TTOU", "--ou"},
		{"1", "--1"},
		{"usr1", "--1"},
		{"2", "--2"},
		{"usr2", "--2"},
		{"w", "--w"},
		{"winch", "--w"},
	}

	status := &Status{}
	for _, s := range testSignals {
		if err := getJSON(fmt.Sprintf("/signal/%s", s.signal), status); err != nil {
			t.Fatal(err)
		}
		data, err := ioutil.ReadFile(tmpfile.Name())
		if err != nil {
			t.Fatal(err)
		}
		expect(t, s.expected, string(data))
	}

	// test "d", (keep it down and don't restart)
	if err := getJSON("/signal/d", status); err != nil {
		t.Fatal(err)
	}
	// wait for process to finish
	err = <-p.errch
	atomic.StoreUint32(&d.lock, d.lockOnce)
	expect(t, "signal: terminated", err.Error())

	// create error os: process already finished
	mylog.Reset()
	for _, s := range testSignals {
		if err := getJSON(fmt.Sprintf("/signal/%s", s.signal), status); err != nil {
			t.Fatal(err)
		}
		expect(t, true, strings.HasSuffix(strings.TrimSpace(mylog.String()), "os: process already finished"))
		mylog.Reset()
	}

	if err := getJSON("/signal/d", status); err != nil {
		t.Fatal(err)
	}
	expect(t, true, strings.HasSuffix(strings.TrimSpace(mylog.String()), "os: process already finished"))

	if err := getJSON("/signal/t", status); err != nil {
		t.Fatal(err)
	}
	expect(t, true, strings.HasSuffix(strings.TrimSpace(mylog.String()), "os: process already finished"))

	if err := getJSON("/signal/p", status); err != nil {
		t.Fatal(err)
	}
	expect(t, true, strings.HasSuffix(strings.TrimSpace(mylog.String()), "os: process already finished"))

	if err := getJSON("/signal/x", status); err != nil {
		t.Fatal(err)
	}
}
