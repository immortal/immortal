package immortal

import (
	"bufio"
	"bytes"
	"fmt"
	"io"
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
	tmpdir := os.Getenv("TEST_TEMP_DIR")
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
	fifo, err := OpenFifo(filepath.Join(tmpdir, "fifo"))
	if err != nil {
		panic(err)
	}
	defer fifo.Close()
	for {
		signalType := <-c
		switch signalType {
		case syscall.SIGALRM:
			fmt.Fprintln(fifo, "--a")
		case syscall.SIGCONT:
			fmt.Fprintln(fifo, "--c")
		case syscall.SIGHUP:
			fmt.Fprintln(fifo, "--h")
		case syscall.SIGINT:
			fmt.Fprintln(fifo, "--i")
		case syscall.SIGQUIT:
			fmt.Fprintln(fifo, "--q")
		case syscall.SIGTTIN:
			fmt.Fprintln(fifo, "--in")
		case syscall.SIGTTOU:
			fmt.Fprintln(fifo, "--ou")
		case syscall.SIGUSR1:
			fmt.Fprintln(fifo, "--1")
		case syscall.SIGUSR2:
			fmt.Fprintln(fifo, "--2")
		case syscall.SIGWINCH:
			fmt.Fprintln(fifo, "--w")
		}
	}
}

func TestSignalsFiFo(t *testing.T) {
	sdir, err := ioutil.TempDir("", "TestSignalsFiFo")
	if err != nil {
		t.Error(err)
	}
	defer os.RemoveAll(sdir)
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
	tmpdir, err := ioutil.TempDir("", "signals")
	if err != nil {
		t.Fatal(err)
	}
	defer os.Remove(tmpdir) // clean up

	cfg := &Config{
		Env:     map[string]string{"GO_WANT_HELPER_PROCESS": "1", "TEST_TEMP_DIR": tmpdir},
		command: []string{filepath.Join(dirBase, base), "-test.run=TestHelperProcessSignalsFiFo", "--"},
		Cwd:     parentDir,
		Pid: Pid{
			Parent: filepath.Join(parentDir, "parent.pid"),
			Child:  filepath.Join(parentDir, "child.pid"),
		},
		ctl: sdir,
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

	// Make fifo in/out
	if err := MakeFifo(filepath.Join(tmpdir, "fifo")); err != nil {
		t.Fatal(err)
	}

	// sync fifo
	time.Sleep(time.Second)

	// open fifo for reading
	fifo, err := OpenFifo(filepath.Join(tmpdir, "fifo"))
	if err != nil {
		t.Error(err)
	}

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

	type Response struct {
		Err string
	}
	res := &Response{}

	for _, s := range testSignals {
		if err := getJSON(sdir, fmt.Sprintf("/signal/%s", s.signal), res); err != nil {
			t.Fatal(err)
		}
		expect(t, "", res.Err)
		waitSig(t, fifo, s.expected)
	}

	// test "d", (keep it down and don't restart)
	if err := getJSON(sdir, "/signal/d", res); err != nil {
		t.Fatal(err)
	}
	// wait for process to finish
	err = <-p.errch
	atomic.StoreUint32(&d.lock, d.lockOnce)
	expect(t, "signal: terminated", err.Error())

	// create error os: process already finished
	mylog.Reset()
	for _, s := range testSignals {
		if err := getJSON(sdir, fmt.Sprintf("/signal/%s", s.signal), res); err != nil {
			t.Fatal(err)
		}
		expect(t, true, strings.HasSuffix(strings.TrimSpace(mylog.String()), "os: process already finished"))
		mylog.Reset()
	}

	if err := getJSON(sdir, "/signal/d", res); err != nil {
		t.Fatal(err)
	}
	expect(t, true, strings.HasSuffix(strings.TrimSpace(mylog.String()), "os: process already finished"))

	if err := getJSON(sdir, "/signal/t", res); err != nil {
		t.Fatal(err)
	}
	expect(t, true, strings.HasSuffix(strings.TrimSpace(mylog.String()), "os: process already finished"))

	if err := getJSON(sdir, "/signal/p", res); err != nil {
		t.Fatal(err)
	}
	expect(t, true, strings.HasSuffix(strings.TrimSpace(mylog.String()), "os: process already finished"))

	if err := getJSON(sdir, "/signal/unknown", res); err != nil {
		t.Fatal(err)
	}
	expect(t, "Unknown signal: unknown", res.Err)

	if err := getJSON(sdir, "/signal/x", res); err != nil {
		t.Fatal(err)
	}
}

func waitSig(t *testing.T, fifo *os.File, sig string) {
	buf := make([]byte, 0, 8)
	r := bufio.NewReader(fifo)
	for {
		n, err := r.Read(buf[:cap(buf)])
		if n == 0 {
			if err == nil {
				continue
			}
			if err == io.EOF {
				continue
			}
			t.Fatal(err)
		}
		buf = buf[:n]
		msg := strings.TrimSpace(string(buf))
		if msg != sig {
			expect(t, sig, msg)
		}
		return
	}
}
