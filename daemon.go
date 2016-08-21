package immortal

import (
	"os"
	"path/filepath"
	"sync/atomic"
	"syscall"
	"time"
)

type Daemon struct {
	*Config
	*Control
	Forker
	Process
	lock       uint32
	lock_defer uint32
	count      uint64
	running    chan struct{}
}

func (self *Daemon) Run() {
	if atomic.SwapUint32(&self.lock, uint32(1)) != 0 {
		return
	}

	// increment count by 1
	atomic.AddUint64(&self.count, 1)

	if self.Wait > 0 {
		time.Sleep(time.Duration(self.Wait) * time.Second)
	}

	self.Process = NewProcess(self.Config)
	self.running = make(chan struct{})
	go func(done chan<- error, r chan<- struct{}) {
		done <- self.Process.Exec(self.Config, r)
		// lock_defer defaults to 0, 1 to run only once/down (don't restart)
		atomic.StoreUint32(&self.lock, self.lock_defer)
		close(r)
		r = nil
	}(self.Control.done, self.running)
}

func New(cfg *Config) (*Daemon, error) {
	var (
		supDir string
		err    error
	)

	if cfg.Cwd != "" {
		supDir = filepath.Join(cfg.Cwd, "supervise")
	} else {
		d, err := os.Getwd()
		if err != nil {
			return nil, err
		}
		supDir = filepath.Join(d, "supervise")
	}

	control := &Control{
		fifo: make(chan Return),
		quit: make(chan struct{}),
		done: make(chan error),
	}

	// if ctrl create supervise dir
	if cfg.ctrl {
		// create fifo
		var ctrl = []string{"control", "ok"}
		for _, v := range ctrl {
			if err := MakeFifo(filepath.Join(supDir, v)); err != nil {
				return nil, err
			}
		}

		// lock
		if lock, err := os.Create(filepath.Join(supDir, "lock")); err != nil {
			return nil, err
		} else if err = syscall.Flock(int(lock.Fd()), syscall.LOCK_EX+syscall.LOCK_NB); err != nil {
			return nil, err
		}

		// read fifo
		if control.fifo_control, err = OpenFifo(filepath.Join(supDir, "control")); err != nil {
			return nil, err
		}
		if control.fifo_ok, err = OpenFifo(filepath.Join(supDir, "ok")); err != nil {
			return nil, err
		}
	}

	return &Daemon{
		Config:  cfg,
		Control: control,
		Forker:  &Fork{},
	}, nil
}
