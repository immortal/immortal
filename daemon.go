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
	lock       uint32
	lock_defer uint32
	start      time.Time
	cmd        *exec.Cmd
}

func (self *Daemon) Process() *os.Process {
	return self.cmd.Process
}

func (self *Daemon) Run() {
	if atomic.SwapUint32(&self.lock, uint32(1)) != 0 {
		if self.cmd == nil {
			log.Printf("Service down")
		} else {
			log.Printf("PID %d (WANT IT DOWN) FIX THIS", self.cmd.Process.Pid)
			//log.Println("FIX THIS")
		}
		return
	}

	// Command to execute
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

	if err := self.cmd.Start(); err != nil {
		self.Control.state <- err
		return
	}

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
		if err := self.WritePid(self.Pid.Child, self.cmd.Process.Pid); err != nil {
			log.Print(err)
		}
	}

	go func() {
		defer func() {
			if self.Logger.IsLogging() {
				w.Close()
			}
			// lock_defer defaults to 0, 1 to run only once/down (don't restart)
			atomic.StoreUint32(&self.lock, self.lock_defer)
		}()
		self.Control.state <- self.cmd.Wait()
	}()
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
		fifo:  make(chan Return),
		quit:  make(chan struct{}),
		state: make(chan error),
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
