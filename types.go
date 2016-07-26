package immortal

import (
	"log"
	"os"
	"os/user"
)

type Configuration interface {
	Exists(path string) bool
	IsExec(path string) bool
}

type Setup struct{}

func (self *Setup) Exists(path string) bool {
	if _, err := os.Stat(path); os.IsNotExist(err) {
		return false
	}
	return true
}

func (self *Setup) IsExec(path string) bool {
	return true
}

type Daemon struct {
	command     []string
	count       uint32
	count_defer uint32
	ctrl        Ctrl
	log         bool
	logger      *log.Logger
	owner       *user.User
	process     *os.Process
	run         Run
}

type Run struct {
	Command   string
	Ctrl      bool
	Cwd       string
	Env       map[string]string
	Logfile   string
	Logger    string
	User      string
	ParentPid string
	ChildPid  string
	FollowPid string
}

type Ctrl struct {
	fifo         chan Return
	quit         chan struct{}
	state        chan error
	control_fifo *os.File
	status_fifo  *os.File
}

type Return struct {
	err error
	msg string
}
