package immortal

import (
	"fmt"
	"io/ioutil"
	"log"
	"os"
	"os/user"
	"path/filepath"
	"sync/atomic"
	"syscall"
	"time"
)

// Daemon struct
type Daemon struct {
	cfg            *Config
	count          uint64
	lock, lockOnce uint32
	process        *process
	quit           chan struct{}
	sTime          time.Time
	supDir         string
}

// Run returns a process instance
func (d *Daemon) Run(p Process) (*process, error) {
	if atomic.SwapUint32(&d.lock, uint32(1)) != 0 {
		return nil, fmt.Errorf("lock: %d lock once: %d", d.lock, d.lockOnce)
	}

	var err error

	// increment count by 1
	atomic.AddUint64(&d.count, 1)

	time.Sleep(time.Duration(d.cfg.Wait) * time.Second)

	d.process, err = p.Start()
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

	return d.process, nil
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
	var supDir string

	// create supervise directory in current directory
	if cfg.ctl {
		d, err := os.Getwd()
		if err != nil {
			return nil, err
		}
		supDir = filepath.Join(d, "supervise")
	} else {
		// create an .immotal dir on HOME user when calling immortal directly
		// and not using immortal-dir, this helps to run immortal-ctl and
		// check status of all daemons
		usr, err := user.Current()
		if err != nil {
			return nil, err
		}
		supDir = filepath.Join(usr.HomeDir,
			".immortal",
			fmt.Sprintf("%d", os.Getpid()),
			"supervise")
	}

	// create supervise dir
	if err := os.MkdirAll(supDir, os.ModePerm); err != nil {
		return nil, err
	}

	// lock
	if lock, err := os.Create(filepath.Join(supDir, "lock")); err != nil {
		return nil, err
	} else if err = syscall.Flock(int(lock.Fd()), syscall.LOCK_EX+syscall.LOCK_NB); err != nil {
		return nil, err
	}

	// remove previous socket in case exists
	os.Remove(filepath.Join(supDir, "immortal.sock"))

	return &Daemon{
		cfg:    cfg,
		supDir: supDir,
		quit:   make(chan struct{}),
		sTime:  time.Now(),
	}, nil
}
