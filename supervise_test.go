package immortal

import (
	"fmt"
	"io/ioutil"
	"log"
	"os"
	"os/exec"
	"path/filepath"
	"strconv"
	"sync"
	"testing"
	"time"
)

func TestSupervise(t *testing.T) {
	sdir, err := ioutil.TempDir("", "TestSupervise")
	if err != nil {
		t.Error(err)
	}
	defer os.RemoveAll(sdir)
	log.SetOutput(ioutil.Discard)
	log.SetFlags(0)
	tmpfile, err := ioutil.TempFile(sdir, "follow.pid")
	if err != nil {
		t.Error(err)
	}
	cfg := &Config{
		Env:     map[string]string{"GO_WANT_HELPER_PROCESS": "sleep10"},
		command: []string{os.Args[0]},
		Cwd:     sdir,
		ctl:     sdir,
		Pid: Pid{
			Parent: filepath.Join(sdir, "parent.pid"),
			Child:  filepath.Join(sdir, "child.pid"),
			Follow: tmpfile.Name(),
		},
		Retries: -1,
	}
	// prettyPrint cfg
	// fmt.Println(prettyPrint(cfg))

	// create new daemon
	d, err := New(cfg)
	if err != nil {
		t.Fatal(err)
	}

	// create socket
	if err := d.Listen(); err != nil {
		t.Fatal(err)
	}

	//go Supervise(d)
	var wg sync.WaitGroup
	wg.Add(1)
	go func() {
		defer wg.Done()
		Supervise(d)
	}()

	// read child pid
	time.Sleep(time.Second)
	childPid, err := d.ReadPidFile(filepath.Join(sdir, "child.pid"))
	if err != nil {
		t.Error(err)
	}
	d.RLock()
	expect(t, 1, d.count)
	d.RUnlock()

	// terminate process a new child pid should be created
	status := &Status{}
	if err := GetJSON(filepath.Join(sdir, "immortal.sock"), "/signal/t", status); err != nil {
		t.Fatal(err)
	}

	// read new child pid should be different from previous one
	time.Sleep(time.Second)
	newchildPid, err := d.ReadPidFile(filepath.Join(sdir, "child.pid"))
	if err != nil {
		t.Error(err)
	}
	d.RLock()
	expect(t, 2, d.count)
	d.RUnlock()

	if childPid == newchildPid {
		t.Error("Expecting new child pid")
	}

	// follow pid should be false
	d.RLock()
	expect(t, false, d.fpid)
	d.RUnlock()

	// fake watch pid with other process fpid should be true
	cmd := exec.Command("sleep", "1")
	cmd.Start()
	go func() {
		cmd.Wait()
	}()
	watchPid := cmd.Process.Pid
	err = ioutil.WriteFile(tmpfile.Name(), []byte(strconv.Itoa(watchPid)), 0644)
	if err != nil {
		t.Error(err)
	}

	// terminate the process and pid should be followed
	if err := GetJSON(filepath.Join(sdir, "immortal.sock"), "/signal/t", status); err != nil {
		t.Fatal(err)
	}
	for d.IsRunning(watchPid) {
		// wait mock watchpid to finish
		time.Sleep(500 * time.Millisecond)
		d.RLock()
		expect(t, true, d.fpid)
		d.RUnlock()
	}

	// a new process/child should be created
	time.Sleep(time.Second)
	newchildPidAfter, err := d.ReadPidFile(filepath.Join(sdir, "child.pid"))
	if err != nil {
		t.Error(err)
	}
	if newchildPid == newchildPidAfter {
		t.Error("Expecting different pids")
	}
	d.RLock()
	expect(t, 3, d.count)
	d.RUnlock()

	// exit supervisor
	// TODO - set environment variable IMMORTAL_EXIT to exit
	GetJSON(filepath.Join(sdir, "immortal.sock"), "/signal/exit", status)
	close(d.quit)
	wg.Wait()
}

// TestSuperviseWait will test that the wait variable in supervise.go is set to
// approximately 1 second (wait = time.Second - uptime) to avoid high CPU usage
func TestSuperviseWait(t *testing.T) {
	sdir, err := ioutil.TempDir("", "TestSuperviseWait")
	if err != nil {
		t.Error(err)
	}
	defer os.RemoveAll(sdir)
	log.SetOutput(ioutil.Discard)
	log.SetFlags(0)
	cfg := &Config{
		Env:     map[string]string{"GO_WANT_HELPER_PROCESS": "nosleep"},
		command: []string{os.Args[0]},
		Cwd:     sdir,
		ctl:     sdir,
		Pid: Pid{
			Parent: filepath.Join(sdir, "parent.pid"),
			Child:  filepath.Join(sdir, "child.pid"),
		},
		Retries: -1,
	}
	// create new daemon
	d, err := New(cfg)
	if err != nil {
		t.Fatal(err)
	}
	// create socket
	if err := d.Listen(); err != nil {
		t.Fatal(err)
	}
	var wg sync.WaitGroup
	wg.Add(1)
	go func() {
		defer wg.Done()
		Supervise(d)
	}()

	status := &Status{}
	for status.Count < 3 {
		time.Sleep(500 * time.Millisecond)
		GetJSON(filepath.Join(sdir, "immortal.sock"), "/", status)
		//	fmt.Printf("prettyPrint(status) = %+v\n", prettyPrint(status))
	}
	GetJSON(filepath.Join(sdir, "immortal.sock"), "/signal/exit", status)

	close(d.quit)
	d.RLock()
	expect(t, true, d.count >= 2)
	d.RUnlock()

	wg.Wait()
}

func TestRetries(t *testing.T) {
	sdir, err := ioutil.TempDir("", "TestRetries")
	if err != nil {
		t.Error(err)
	}
	defer os.RemoveAll(sdir)
	log.SetOutput(ioutil.Discard)
	log.SetFlags(0)

	var tt = []struct {
		retry  int
		expect int
	}{
		{0, 1},
		{1, 2},
		{2, 3},
	}
	for _, tc := range tt {
		rsdir := filepath.Join(sdir, fmt.Sprintf("%d", tc.retry))
		if err := os.Mkdir(rsdir, os.ModePerm); err != nil {
			t.Fatal(err)
		}
		t.Run(fmt.Sprintf("retry_%d", tc.retry), func(t *testing.T) {
			cfg := &Config{
				Env:     map[string]string{"GO_WANT_HELPER_PROCESS": "nosleep"},
				command: []string{os.Args[0]},
				Cwd:     rsdir,
				ctl:     rsdir,
				Retries: tc.retry,
			}
			d, err := New(cfg)
			if err != nil {
				t.Fatal(err)
			}
			// create socket
			if err := d.Listen(); err != nil {
				t.Fatal(err)
			}
			// start supervisor
			var wg sync.WaitGroup
			wg.Add(1)
			go func() {
				defer wg.Done()
				Supervise(d)
			}()
			status := &Status{}
			for status.Count < tc.expect {
				err := GetJSON(filepath.Join(rsdir, "immortal.sock"), "/", status)
				if err != nil {
					t.Fatal(err)
				}
				time.Sleep(200 * time.Millisecond)
			}
			close(d.quit)
			wg.Wait()
			d.RLock()
			expect(t, tc.expect, d.count)
			d.RUnlock()
		})
	}
}
