package immortal

import (
	"fmt"
	"io"
	"io/ioutil"
	"log"
	"os"
	"os/exec"
	"path/filepath"
	"strconv"
	"sync/atomic"
	"syscall"
	"time"
)

type Daemon struct {
	*Config
	*Control
	Forker
	Logger
	Process
	count uint64
}

func (self *Daemon) Run() {
	if atomic.SwapUint32(&self.lock, uint32(1)) != 0 {
		return
	}

	// increment count by 1
	atomic.AddUint64(&self.count, 1)

	self.cmd = exec.Command(self.command[0], self.command[1:]...)

	// change working directory
	if self.Cwd != "" {
		self.cmd.Dir = self.Cwd
	}

	// set environment vars
	if self.Env != nil {
		env := os.Environ()
		for k, v := range self.Env {
			env = append(env, fmt.Sprintf("%s=%s", k, v))
		}
		self.cmd.Env = env
	}

	sysProcAttr := new(syscall.SysProcAttr)

	// set owner
	if self.user != nil {
		uid, err := strconv.Atoi(self.user.Uid)
		if err != nil {
			self.Control.state <- err
			return
		}

		gid, err := strconv.Atoi(self.user.Gid)
		if err != nil {
			self.Control.state <- err
			return
		}

		// https://golang.org/pkg/syscall/#SysProcAttr
		sysProcAttr.Credential = &syscall.Credential{
			Uid: uint32(uid),
			Gid: uint32(gid),
		}
	}

	// Set process group ID to Pgid, or, if Pgid == 0, to new pid
	sysProcAttr.Setpgid = true
	sysProcAttr.Pgid = 0

	// set the attributes
	self.cmd.SysProcAttr = sysProcAttr

	// log only if are available loggers
	var (
		r *io.PipeReader
		w *io.PipeWriter
	)
	if self.Logger.IsLogging() {
		r, w = io.Pipe()
		self.cmd.Stdout = w
		self.cmd.Stderr = w
		go self.Logger.StdHandler(r)
	} else {
		self.cmd.Stdin = nil
		self.cmd.Stdout = nil
		self.cmd.Stderr = nil
	}

	// wait N seconds before starting
	if self.Wait > 0 {
		time.Sleep(time.Duration(self.Wait) * time.Second)
	}

	self.running = make(chan struct{})
	go func(cmd *exec.Cmd, running chan<- struct{}) {
		defer func() {
			if self.Logger.IsLogging() {
				w.Close()
			}
			// lock_defer defaults to 0, 1 to run only once/down (don't restart)
			atomic.StoreUint32(&self.lock, self.lock_defer)
		}()

		if err := cmd.Start(); err != nil {
			self.Control.state <- err
			return
		}
		running <- struct{}{}

		// set start time
		self.start = time.Now()

		// write parent pid
		if self.Pid.Parent != "" {
			if err := self.WritePid(self.Pid.Parent, os.Getpid()); err != nil {
				log.Print(err)
			}
		}

		// write child pid
		if self.Pid.Child != "" {
			if err := self.WritePid(self.Pid.Child, cmd.Process.Pid); err != nil {
				log.Print(err)
			}
		}

		self.Control.state <- cmd.Wait()
	}(self.cmd, self.running)
}

func (self *Daemon) WritePid(file string, pid int) error {
	if err := ioutil.WriteFile(file, []byte(fmt.Sprintf("%d", pid)), 0644); err != nil {
		return err
	}
	return nil
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
		state:   make(chan error),
		started: make(chan struct{}, 1),
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
		Logger: &LogWriter{
			logger: NewLogger(cfg),
		},
	}, nil
}
