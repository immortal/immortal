package immortal

import (
	"os"
)

type ProcessContainer interface {
	GetPid() int
	Kill() error
	SetPid(int)
	SetProcess(p *os.Process)
	Signal(sig os.Signal) error
}

type Process struct {
	*os.Process
}

func (self *Process) GetPid() int {
	return self.Pid
}

func (self *Process) SetPid(pid int) {
	self.Pid = pid
}

func (self *Process) SetProcess(p *os.Process) {
	self.Process = p
}
