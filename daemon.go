package immortal

import (
	"fmt"
	"io/ioutil"
	"log"
	"os"
	"path/filepath"
	"sync/atomic"
	"syscall"
	"time"
)

// Return struct used for fifo channel
type Return struct {
	err error
	msg string
}

// Daemon struct
type Daemon struct {
	cfg                 *Config
	count               uint64
	fifo                chan Return
	fifoControl, fifoOk *os.File
	lock, lockOnce      uint32
	quit                chan struct{}
	sTime               time.Time
}

// Run returns a process instance
func (d *Daemon) Run(p Process) (*process, error) {
	if atomic.SwapUint32(&d.lock, uint32(1)) != 0 {
		return nil, fmt.Errorf("lock: %d lock once: %d", d.lock, d.lockOnce)
	}

	// increment count by 1
	atomic.AddUint64(&d.count, 1)

	time.Sleep(time.Duration(d.cfg.Wait) * time.Second)

	process, err := p.Start()
	if err != nil {
		atomic.StoreUint32(&d.lock, d.lockOnce)
		return nil, err
	}

	// write parent pid
	if d.cfg.Pid.Parent != "" {
		if err := d.WritePid(d.cfg.Pid.Parent, os.Getpid()); err != nil {
			log.Println(err)
		}
	}

	// write child pid
	if d.cfg.Pid.Child != "" {
		if err := d.WritePid(d.cfg.Pid.Child, p.Pid()); err != nil {
			log.Println(err)
		}
	}

	return process, nil
}

// WritePid write pid to file
func (d *Daemon) WritePid(file string, pid int) error {
	if err := ioutil.WriteFile(file, []byte(fmt.Sprintf("%d", pid)), 0644); err != nil {
		return err
	}
	return nil
}

// New creates a new daemon
func New(cfg *Config) (*Daemon, error) {
	var (
		err                 error
		fifoControl, fifoOk *os.File
		supDir              string
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
		if fifoControl, err = OpenFifo(filepath.Join(supDir, "control")); err != nil {
			return nil, err
		}
		if fifoOk, err = OpenFifo(filepath.Join(supDir, "ok")); err != nil {
			return nil, err
		}
	}

	return &Daemon{
		cfg:         cfg,
		fifo:        make(chan Return),
		fifoControl: fifoControl,
		fifoOk:      fifoOk,
		quit:        make(chan struct{}),
		sTime:       time.Now(),
	}, nil
}
