package immortal

import (
	//	"fmt"
	"os"
	"reflect"
	"syscall"
	"testing"
)

/* Test Helpers */
func expect(t *testing.T, a interface{}, b interface{}) {
	if a != b {
		t.Errorf("Expected: %v (type %v)  Got: %v (type %v)", a, reflect.TypeOf(a), b, reflect.TypeOf(b))
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
	close(self.wait)
}

func (self *catchSignals) Kill() error {
	return nil
}

func (self *catchSignals) Signal(sig os.Signal) error {
	process, _ := os.FindProcess(self.Pid)
	if err := process.Signal(syscall.Signal(0)); err != nil {
		return err
	}
	self.signal <- sig
	return nil
}
