package immortal

import (
	"fmt"
	"io/ioutil"
	"log"
	"os"
	"os/user"
	"path/filepath"
	"strconv"
	"strings"
	"sync/atomic"
	"syscall"
	"time"
)

// Daemon struct
type Daemon struct {
	cfg                   *Config
	count, lock, lockOnce uint32
	fpid                  bool
	process               *process
	quit, run             chan struct{}
	sTime                 time.Time
	supDir                string
}

// Run returns a process instance
func (d *Daemon) Run(p Process) (*process, error) {
	var err error

	// return if process is running
	if atomic.SwapUint32(&d.lock, uint32(1)) != 0 {
		return nil, fmt.Errorf("cannot start, process still running")
	}

	// increment count by 1
	atomic.AddUint32(&d.count, 1)

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
	lines := strings.Split(string(content), "\n")
	pid, err := strconv.Atoi(lines[0])
	if err != nil {
		return 0, err
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
		home := os.Getenv("HOME")
		if home == "" {
			usr, err := user.Current()
			if err != nil {
				return nil, err
			}
			home = usr.HomeDir
		}
		supDir = filepath.Join(home,
			".immortal",
			fmt.Sprintf("%d", os.Getpid()))
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
