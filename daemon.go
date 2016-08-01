package immortal

import (
	"fmt"
	"io"
	"log"
	"os"
	"os/exec"
	"path/filepath"
	"strconv"
	"sync/atomic"
	"syscall"
)

type Immortal interface {
	Logger
	Daemonizer
	Controller
	Supervise()
	WatchPid(pid int)
}

type Daemonizer interface {
	Fork()
	Run()
	FiFo(f FIFOer) error
}

type Daemon struct {
	*Config
	*Control
	Forker
	Logger
	Supervisor
	Watcher
	count       uint32
	count_defer uint32
	process     *os.Process
	supDir      string
}

func (self *Daemon) FiFo(f FIFOer) error {
	if self.ctrl {
		var ctrl = []string{"control", "ok"}
		if self.Cwd != "" {
			self.supDir = filepath.Join(self.Cwd, "supervise")
		} else {
			d, err := os.Getwd()
			if err != nil {
				return err
			}
			self.supDir = filepath.Join(d, "supervise")
		}
		for k, v := range ctrl {
			ctrl[k] = filepath.Join(self.supDir, v)
		}
		for _, v := range ctrl {
			err := f.Make(v)
			if err != nil {
				return err
			}
		}
	}
	return nil
}

func (self *Daemon) Run() {
	if atomic.SwapUint32(&self.count, uint32(1)) != 0 {
		log.Printf("PID: %d running", self.process.Pid)
		return
	}
	cmd := exec.Command(self.command[0], self.command[1:]...)

	// change working directory
	if self.Cwd != "" {
		cmd.Dir = self.Cwd
	}

	// set environment vars
	if self.Env != nil {
		env := os.Environ()
		for k, v := range self.Env {
			env = append(env, fmt.Sprintf("%s=%s", k, v))
		}
		cmd.Env = env
	}

	sysProcAttr := new(syscall.SysProcAttr)

	// set owner
	if self.user != nil {
		uid, err := strconv.Atoi(self.user.Uid)
		if err != nil {
			self.Control.state <- err
		}

		gid, err := strconv.Atoi(self.user.Gid)
		if err != nil {
			self.Control.state <- err
		}

		//	https://golang.org/pkg/syscall/#SysProcAttr
		sysProcAttr.Credential = &syscall.Credential{
			Uid: uint32(uid),
			Gid: uint32(gid),
		}
	}

	// Set process group ID to Pgid, or, if Pgid == 0, to new pid
	sysProcAttr.Setpgid = true
	sysProcAttr.Pgid = 0

	cmd.SysProcAttr = sysProcAttr

	var (
		r *io.PipeReader
		w *io.PipeWriter
	)
	//	if self.Log.file != "" || self.Logger != "" {
	if self.log {
		r, w = io.Pipe()
		cmd.Stdout = w
		cmd.Stderr = w
		go self.Logger.StdHandler(r)
	} else {
		cmd.Stdin = nil
		cmd.Stdout = nil
		cmd.Stderr = nil
	}

	go func() {
		// count_defer defaults to 0, 1 to run only once/down (don't restart)
		defer func() {
			atomic.StoreUint32(&self.count, self.count_defer)
		}()

		if self.Log.File != "" {
			defer w.Close()
		}

		if err := cmd.Start(); err != nil {
			self.Control.Send(err)
		}

		self.process = cmd.Process

		// follow pid
		if self.Pid.Follow != "" {
			//			go self.watchPid(self.process.Pid)
		}

		// write parent pid
		if self.Pid.Parent != "" {
			if err := WritePid(self.Pid.Parent, os.Getpid()); err != nil {
				log.Print(err)
			}
		}

		// write child pid
		if self.Pid.Child != "" {
			if err := WritePid(self.Pid.Child, self.process.Pid); err != nil {
				log.Print(err)
			}
		}

		//self.Control.state <- cmd.Wait()
		self.Control.Send(cmd.Wait())
	}()
}

func New(cfg *Config) *Daemon {
	return &Daemon{
		Config: cfg,
		Control: &Control{
			fifo:  make(chan Return),
			quit:  make(chan struct{}),
			state: make(chan error),
		},
		Forker: &Fork{},
		Logger: &LogWriter{
			logger: NewLogger(cfg),
		},
		Supervisor: &Sup{},
	}
}
