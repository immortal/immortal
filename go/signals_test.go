package immortal

import (
	"bufio"
	"fmt"
	"io"
	"io/ioutil"
	"log"
	"os"
	"path/filepath"
	"strings"
	"sync/atomic"
	"testing"
	"time"
)

func TestSignalsFiFo(t *testing.T) {
	sdir, err := ioutil.TempDir("", "TestSignalsFiFo")
	if err != nil {
		t.Error(err)
	}
	defer os.RemoveAll(sdir)
	var mylog myBuffer
	log.SetOutput(&mylog)
	log.SetFlags(0)
	// for writing the signals
	tmpdir, err := ioutil.TempDir("", "signals")
	if err != nil {
		t.Fatal(err)
	}
	defer os.Remove(tmpdir) // clean up

	cfg := &Config{
		Env:     map[string]string{"GO_WANT_HELPER_PROCESS": "signalsFiFo", "TEST_TEMP_DIR": tmpdir},
		command: []string{os.Args[0]},
		Cwd:     sdir,
		Pid: Pid{
			Parent: filepath.Join(sdir, "parent.pid"),
			Child:  filepath.Join(sdir, "child.pid"),
		},
		ctl: sdir,
	}
	// create new daemon
	d, err := New(cfg)
	if err != nil {
		t.Error(err)
	}
	// create new process
	p, err := d.Run(NewProcess(cfg))
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

	for _, tc := range testSignals {
		t.Run(tc.signal, func(t *testing.T) {
			if err := GetJSON(filepath.Join(sdir, "immortal.sock"), fmt.Sprintf("/signal/%s", tc.signal), res); err != nil {
				t.Fatal(err)
			}
			expect(t, "", res.Err)
			waitSig(t, fifo, tc.expected)
		})
	}

	// test "d", (keep it down and don't restart)
	if err := GetJSON(filepath.Join(sdir, "immortal.sock"), "/signal/d", res); err != nil {
		t.Fatal(err)
	}
	// wait for process to finish
	err = <-p.errch
	atomic.StoreUint32(&d.lock, d.lockOnce)
	expect(t, "signal: terminated", err.Error())

	// create error os: process already finished
	mylog.Reset()
	for _, tc := range testSignals {
		t.Run(tc.signal, func(t *testing.T) {
			if err := GetJSON(filepath.Join(sdir, "immortal.sock"), fmt.Sprintf("/signal/%s", tc.signal), res); err != nil {
				t.Fatal(err)
			}
			expect(t, true, strings.HasSuffix(strings.TrimSpace(mylog.String()), "no such process"))
			mylog.Reset()
		})
	}

	if err := GetJSON(filepath.Join(sdir, "immortal.sock"), "/signal/d", res); err != nil {
		t.Fatal(err)
	}
	expect(t, true, strings.HasSuffix(strings.TrimSpace(mylog.String()), "no such process"))

	if err := GetJSON(filepath.Join(sdir, "immortal.sock"), "/signal/t", res); err != nil {
		t.Fatal(err)
	}
	expect(t, true, strings.HasSuffix(strings.TrimSpace(mylog.String()), "no such process"))

	if err := GetJSON(filepath.Join(sdir, "immortal.sock"), "/signal/unknown", res); err != nil {
		t.Fatal(err)
	}
	expect(t, "unknown signal: unknown", res.Err)

	if err := GetJSON(filepath.Join(sdir, "immortal.sock"), "/signal/halt", res); err != nil {
		t.Fatal(err)
	}

	// wait for socket to be close
	d.wg.Wait()
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
