package immortal

import (
	"os"
)

type Immortal interface {
	Controller
	Fork()
	Run()
	Supervise()
	WatchPid(pid int)
}

type Return struct {
	err error
	msg string
}

type Ctrl struct {
	fifo  chan Return
	quit  chan struct{}
	state chan error
}

type Controller interface{}

type Daemon struct {
	*Config
	*Ctrl
	Controller
	FIFOer
	Forker
	Runner
	Supervisor
	Watcher
	count       uint32
	count_defer uint32
	process     *os.Process
}

func New(cfg *Config) *Daemon {
	return &Daemon{
		Config: cfg,
		Ctrl: &Ctrl{
			fifo:  make(chan Return),
			quit:  make(chan struct{}),
			state: make(chan error),
		},
		FIFOer:     &FIFO{},
		Forker:     &Fork{},
		Runner:     &Run{},
		Supervisor: &Sup{},
	}
}
