package immortal

import (
	//	"fmt"
	"os"
	"reflect"
	"runtime"
	"syscall"
	"testing"
)

/* Test Helpers */
func expect(t *testing.T, a interface{}, b interface{}) {
	_, fn, line, _ := runtime.Caller(1)
	if a != b {
		t.Fatalf("Expected: %v (type %v)  Got: %v (type %v)  in %s:%d", a, reflect.TypeOf(a), b, reflect.TypeOf(b), fn, line)
	}
}

type myFork struct{}

func (self myFork) Fork() (int, error) {
	return 0, nil
}

type catchSignals struct {
	*os.Process
	signal chan os.Signal
	wait   chan struct{}
}

func (self *catchSignals) GetPid() int {
	return self.Pid
}

func (self *catchSignals) SetPid(pid int) {
	self.Pid = pid
}

func (self *catchSignals) SetProcess(p *os.Process) {
	self.Process = p
	self.wait <- struct{}{}
}

func (self *catchSignals) Kill() (err error) {
	println("<--------", self.Process.Pid)
	err = self.Process.Kill()
	if err != nil {
		return
	}
	return
}

func (self *catchSignals) Signal(sig os.Signal) error {
	process, _ := os.FindProcess(self.Pid)
	if err := process.Signal(syscall.Signal(0)); err != nil {
		self.signal <- syscall.SIGILL
		return err
	}
	self.signal <- sig
	return nil
}
