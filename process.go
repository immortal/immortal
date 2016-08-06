package immortal

import (
	"os"
)

type ProcessContainer interface {
	GetPid() int
	SetProcess(p *os.Process)
}

type Process struct {
	*os.Process
}

func (self *Process) GetPid() int {
	return self.Pid
}

func (self *Process) SetProcess(p *os.Process) {
	self.Process = p
}
