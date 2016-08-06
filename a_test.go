package immortal

import (
	"os"
	"reflect"
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
}

func (self *catchSignals) GetPid() int {
	return self.Pid
}

func (self *catchSignals) SetPid(pid int) {
	self.Pid = pid
}

func (self *catchSignals) SetProcess(p *os.Process) {
	self.Process = p
}

func (self *catchSignals) Kill() error {
	return nil
}

func (self *catchSignals) Signal(sig os.Signal) error {
	self.signal <- sig
	return nil
}
