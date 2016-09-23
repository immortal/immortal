package immortal

import (
	"crypto/md5"
	"fmt"
	"io/ioutil"
	"log"
	"os"
	"os/user"
	"path/filepath"
	"strings"
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
	cfg            *Config
	count          uint64
	fifo           chan Return
	supDir         string
	lock, lockOnce uint32
	quit           chan struct{}
	sTime          time.Time
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
	var supDir string

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
			fmt.Sprintf("%x", md5.Sum([]byte(strings.Join(cfg.command, "")))),
			"supervise")
	}

	// create supervise dir
	err := os.MkdirAll(supDir, os.ModePerm)
	if err != nil {
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
		fifo:   make(chan Return),
		supDir: supDir,
		quit:   make(chan struct{}),
		sTime:  time.Now(),
	}, nil
}
