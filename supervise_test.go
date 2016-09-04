package immortal

import (
	"fmt"
	"io/ioutil"
	"log"
	"os"
	"os/exec"
	"os/signal"
	"path/filepath"
	"strconv"
	"sync"
	"syscall"
	"testing"
	"time"
)

func TestHelperProcessSupervise(*testing.T) {
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

func TestHelperProcessSupervise2(*testing.T) {
	if os.Getenv("GO_WANT_HELPER_PROCESS") != "1" {
		return
	}
	os.Exit(0)
}

func TestSupervise(t *testing.T) {
	log.SetOutput(ioutil.Discard)
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
	tmpfile, err := ioutil.TempFile("", "TestPidFile")
	if err != nil {
		t.Error(err)
	}
	defer os.Remove(tmpfile.Name()) // clean up
	cfg := &Config{
		Env:     map[string]string{"GO_WANT_HELPER_PROCESS": "1"},
		command: []string{filepath.Join(dirBase, base), "-test.run=TestHelperProcessSupervise", "--"},
		Cwd:     parentDir,
		ctrl:    true,
		Pid: Pid{
			Parent: filepath.Join(parentDir, "parent.pid"),
			Child:  filepath.Join(parentDir, "child.pid"),
			Follow: tmpfile.Name(),
		},
	}
	// to remove lock
	os.RemoveAll(filepath.Join(parentDir, "supervise"))
	d, err := New(cfg)
	if err != nil {
		t.Fatal(err)
	}
	time.Sleep(time.Second)
	fctrl, err := OpenFifo(filepath.Join(parentDir, "supervise/control"))
	if err != nil {
		t.Fatal(err)
	}
	go Supervise(d)
	defer func() {
		fmt.Fprintln(fctrl, "kill")
		fmt.Fprintln(fctrl, "exit")
	}()

	sup := &Sup{}

	time.Sleep(time.Second)

	// check pids
	parent_pid, err := sup.ReadPidFile(filepath.Join(parentDir, "parent.pid"))
	if err != nil {
		t.Error(err)
	}
	expect(t, os.Getpid(), parent_pid)
	child_pid, err := sup.ReadPidFile(filepath.Join(parentDir, "child.pid"))
	if err != nil {
		t.Error(err)
	}
	expect(t, true, child_pid > 0)

	fmt.Fprintln(fctrl, "t")
	time.Sleep(time.Second)
	newchild_pid, err := sup.ReadPidFile(filepath.Join(parentDir, "child.pid"))
	if err != nil {
		t.Error(err)
	}
	if child_pid == newchild_pid {
		t.Error("Expecting new child pid")
	}

	// test info
	syscall.Kill(parent_pid, syscall.SIGQUIT)
	time.Sleep(time.Second)

	// fake watch pid with other process
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

	// reset
	fmt.Fprintln(fctrl, "t")

	select {
	case <-time.After(5 * time.Second):
		t.Error("time out")
	default:
		for sup.IsRunning(watchPid) {
			// wait mock watchpid to finish
			time.Sleep(2100 * time.Millisecond)
		}
		newchild_pid_after, err := sup.ReadPidFile(filepath.Join(parentDir, "child.pid"))
		if err != nil {
			t.Error(err)
		}
		if newchild_pid == newchild_pid_after {
			t.Error("Expecting different pids")
		}
	}
}

func TestSuperviseWait(t *testing.T) {
	log.SetOutput(ioutil.Discard)
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
		command: []string{filepath.Join(dirBase, base), "-test.run=TestHelperProcessSupervise2", "--"},
		Cwd:     parentDir,
		ctrl:    true,
		Pid: Pid{
			Parent: filepath.Join(parentDir, "parent.pid"),
			Child:  filepath.Join(parentDir, "child.pid"),
		},
	}
	// to remove lock
	os.RemoveAll(filepath.Join(parentDir, "supervise"))
	d, err := New(cfg)
	if err != nil {
		t.Fatal(err)
	}
	time.Sleep(time.Second)
	fctrl, err := OpenFifo(filepath.Join(parentDir, "supervise/control"))
	if err != nil {
		t.Fatal(err)
	}
	var wg sync.WaitGroup
	wg.Add(1)
	go func() {
		defer wg.Done()
		Supervise(d)
	}()
	time.Sleep(2 * time.Second)
	fmt.Fprintln(fctrl, "exit")
	wg.Wait()
	expect(t, true, d.count >= 2)
}
