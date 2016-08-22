package immortal

import (
	"os"
	"path/filepath"
	"sync/atomic"
	"syscall"
	"time"
)

type procStatus struct {
	uptime time.Duration
	ch     chan<- procStatus
}

type Daemon struct {
	*Config
	*Control
	Process
	count      uint64
	ctl        chan interface{}
	lock       uint32
	lock_defer uint32
	start      time.Time
}

func (self *Daemon) Run() {
	if atomic.SwapUint32(&self.lock, uint32(1)) != 0 {
		return
	}

	// increment count by 1
	atomic.AddUint64(&self.count, 1)

	start := time.After(time.Duration(self.Wait) * time.Second)

	for {
		select {
		case ctl := <-self.ctl:
			switch c := ctl.(type) {
			case procStatus:
				c.ch <- self.status()
			}
		case <-start:
			self.Process = NewProcess(self.Config)
			self.Process.Start(self.Config)
			// lock_defer defaults to 0, 1 to run only once/down (don't restart)
			//atomic.StoreUint32(&self.lock, self.lock_defer)
		}
	}
}

func (self *Daemon) Status() procStatus {
	ch := make(chan procStatus, 1)
	self.ctl <- procStatus{ch: ch}
	return <-ch
}

func (self *Daemon) status() procStatus {
	s := procStatus{
		uptime: self.Process.Uptime(),
	}
	return s
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
		fifo:    make(chan Return),
		quit:    make(chan struct{}),
		done:    make(chan error),
		running: make(chan struct{}),
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
		start:   time.Now(),
		ctl:     make(chan interface{}),
	}, nil
}
