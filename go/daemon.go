package immortal

import (
	"bytes"
	"fmt"
	"io/ioutil"
	"log"
	"os"
	"path/filepath"
	"strconv"
	"strings"
	"sync"
	"sync/atomic"
	"syscall"
	"time"
)

// Daemon struct
type Daemon struct {
	sync.RWMutex
	cfg            *Config
	count          int
	fpid           bool
	lock, lockOnce uint32
	process        *process
	quit, run      chan struct{}
	sTime          time.Time
	supDir         string
	wg             sync.WaitGroup
}

// Run returns a process instance
func (d *Daemon) Run(p Process) (*process, error) {
	d.Lock()
	defer d.Unlock()

	var err error

	// return if process is running
	if atomic.SwapUint32(&d.lock, uint32(1)) != 0 {
		return nil, fmt.Errorf("cannot start, process still running or waiting to be started")
	}

	// increment count by 1
	d.count++

	// to print remaininig seconds to start cmd == nil
	d.process = p.GetProcess()

	time.Sleep(time.Duration(d.cfg.Wait) * time.Second)

	if d.process, err = p.Start(); err != nil {
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

	// not following a pid
	d.fpid = false

	return d.process, nil
}

// WritePid write pid to file
func (d *Daemon) WritePid(file string, pid int) error {
	return ioutil.WriteFile(file, []byte(fmt.Sprintf("%d", pid)), 0644)
}

// IsRunning check if process is running
func (d *Daemon) IsRunning(pid int) bool {
	process, _ := os.FindProcess(pid)
	if err := process.Signal(syscall.Signal(0)); err != nil {
		return false
	}
	return true
}

// ReadPidFile read pid from file if error returns pid 0
func (d *Daemon) ReadPidFile(pidfile string) (int, error) {
	content, err := ioutil.ReadFile(pidfile)
	if err != nil {
		return 0, err
	}
	pid, err := strconv.Atoi(string(bytes.TrimSpace(content)))
	if err != nil {
		return 0, fmt.Errorf("error parsing pid from %s: %s", pidfile, err)
	}
	return pid, nil
}

// New creates a new daemon
func New(cfg *Config) (*Daemon, error) {
	var supDir string

	// create supervise directory in specified directory
	// defaults to /var/run/immortal/<app>
	if cfg.ctl != "" {
		supDir = cfg.ctl
	} else {
		// create an .immortal dir on $HOME user when calling immortal directly
		// and not using immortal-dir, this helps to run immortal-ctl and
		// check status of all daemons
		home, err := GetUserSdir()
		if err != nil {
			return nil, err
		}

		if cfg.configFile != "" {
			serviceFile := filepath.Base(cfg.configFile)
			supDir = filepath.Join(home,
				fmt.Sprintf("%s", strings.TrimSuffix(serviceFile, filepath.Ext(serviceFile))))
		} else {
			supDir = filepath.Join(home,
				fmt.Sprintf("%d", os.Getpid()))
		}
	}

	// create supervise dir
	if err := os.MkdirAll(supDir, os.ModePerm); err != nil {
		return nil, err
	}

	// lock
	if lock, err := os.Create(filepath.Join(supDir, "lock")); err != nil {
		return nil, err
	} else if err = syscall.Flock(int(lock.Fd()), syscall.LOCK_EX+syscall.LOCK_NB); err != nil {
		// resource temporarily unavailable
		return nil, err
	}

	// remove previous socket in case exists
	os.Remove(filepath.Join(supDir, "immortal.sock"))

	return &Daemon{
		cfg:    cfg,
		supDir: supDir,
		quit:   make(chan struct{}),
		run:    make(chan struct{}, 1),
		sTime:  time.Now(),
	}, nil
}
