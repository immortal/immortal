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

type Return struct {
	err error
	msg string
}

type Daemon struct {
	cfg          *Config
	count        uint64
	ctrl         chan interface{}
	done         chan error
	fifo         chan Return
	fifo_control *os.File
	fifo_ok      *os.File
	lock         uint32
	lock_once    uint32
	pid          int
	quit         chan struct{}
	sTime        time.Time
}

func (d *Daemon) Run(p Process) {
	if atomic.SwapUint32(&d.lock, uint32(1)) != 0 {
		return
	}

	// increment count by 1
	atomic.AddUint64(&d.count, 1)

	time.Sleep(time.Duration(d.cfg.Wait) * time.Second)

	process, err := p.Start()
	if err != nil {
		return
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

	d.ctrl = make(chan interface{})
	// control process loop
	go d.control(process)
}

// WritePid write pid to file
func (d *Daemon) WritePid(file string, pid int) error {
	if err := ioutil.WriteFile(file, []byte(fmt.Sprintf("%d", pid)), 0644); err != nil {
		return err
	}
	return nil
}

func New(cfg *Config) (*Daemon, error) {
	var (
		err          error
		fifo_control *os.File
		fifo_ok      *os.File
		supDir       string
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
		if fifo_control, err = OpenFifo(filepath.Join(supDir, "control")); err != nil {
			return nil, err
		}
		if fifo_ok, err = OpenFifo(filepath.Join(supDir, "ok")); err != nil {
			return nil, err
		}
	}

	return &Daemon{
		cfg: cfg,
		//		ctrl:         make(chan interface{}),
		done:         make(chan error),
		fifo:         make(chan Return),
		fifo_control: fifo_control,
		fifo_ok:      fifo_ok,
		quit:         make(chan struct{}),
		sTime:        time.Now(),
	}, nil
}
