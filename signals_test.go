package immortal

import (
	"bufio"
	"bytes"
	"fmt"
	"io"
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
	fifo, err := OpenFifo("supervise/control")
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
	cfg := &Config{
		Env:     map[string]string{"GO_WANT_HELPER_PROCESS": "1"},
		command: []string{filepath.Join(dirBase, base), "-test.run=TestHelperProcessSignalsFiFo", "--"},
		Cwd:     parentDir,
		Pid: Pid{
			Parent: filepath.Join(parentDir, "parent.pid"),
			Child:  filepath.Join(parentDir, "child.pid"),
		},
		ctrl: true,
	}
	d, err := New(cfg)
	if err != nil {
		t.Error(err)
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
		t.Error(err)
	} else {
		expect(t, p.Pid(), pid)
	}

	// wait "probably" for fifo to be ready (check this)
	time.Sleep(time.Second)

	sup.ReadFifoControl(d.fifo_control, d.fifo)

	// open fifo for reading
	fifo, err := OpenFifo(filepath.Join(parentDir, "supervise/ok"))
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
	go func() {
		for {
			select {
			case fifo := <-d.fifo:
				sup.HandleSignals(fifo.msg, d)
			}
		}
	}()

	for _, s := range testSignals {
		sup.HandleSignals(s.signal, d)
		waitSig(t, fifo, s.expected)
	}

	// test "d", (keep it down and don't restart)
	sup.HandleSignals("down", d)
	// wait for process to finish
	err = <-p.errch
	atomic.StoreUint32(&d.lock, d.lock_once)
	expect(t, "signal: terminated", err.Error())

	// create error os: process already finished
	mylog.Reset()
	for _, s := range testSignals {
		sup.HandleSignals(s.signal, d)
		expect(t, true, strings.HasSuffix(strings.TrimSpace(mylog.String()), "os: process already finished"))
		mylog.Reset()
	}

	sup.HandleSignals("d", d)
	expect(t, true, strings.HasSuffix(strings.TrimSpace(mylog.String()), "os: process already finished"))
	sup.HandleSignals("t", d)
	expect(t, true, strings.HasSuffix(strings.TrimSpace(mylog.String()), "os: process already finished"))
	sup.HandleSignals("p", d)
	expect(t, true, strings.HasSuffix(strings.TrimSpace(mylog.String()), "os: process already finished"))

	//exit
	sup.HandleSignals("x", d)
	<-d.quit
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
