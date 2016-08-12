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
	process    ProcessContainer
	start      time.Time
}

func (self *Daemon) String() string {
	return fmt.Sprintf("%d", self.process.GetPid())
}

func (self *Daemon) WritePid(file string, pid int) error {
	if err := ioutil.WriteFile(file, []byte(fmt.Sprintf("%d", pid)), 0644); err != nil {
		return err
	}
	return nil
}

func (self *Daemon) Run() {
	if atomic.SwapUint32(&self.lock, uint32(1)) != 0 {
		log.Printf("PID: %d running", self.process.GetPid())
		return
	}

	// set start
	self.start = time.Now()

	// wait N seconds before starting
	if self.Wait > 0 {
		time.Sleep(time.Duration(self.Wait) * time.Second)
	}

	// Command to execute
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
	cmd.SysProcAttr = sysProcAttr

	// log only if are available loggers
	var (
		r *io.PipeReader
		w *io.PipeWriter
	)
	if self.Logger.IsLogging() {
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
		// lock_defer defaults to 0, 1 to run only once/down (don't restart)
		defer func() {
			fmt.Printf("self.lock = %+v\n", self.lock)
			fmt.Printf("self.lock_defer = %+v\n", self.lock_defer)
			atomic.StoreUint32(&self.lock, self.lock_defer)
			println("process ended ........ lock: ", self.lock, " lock_defer: ", self.lock_defer)
			if cmd.ProcessState != nil {
				fmt.Printf("PID %d terminated, %s [%v user  %v sys  %s up]\n", cmd.ProcessState.Pid(), cmd.ProcessState, cmd.ProcessState.UserTime(), cmd.ProcessState.SystemTime(), time.Since(self.start))
			}
			//	self.process = &Process{&os.Process{}}
		}()

		if self.Logger.IsLogging() {
			defer w.Close()
		}

		if err := cmd.Start(); err != nil {
			self.Control.state <- err
			return
		}

		// use the process container
		self.process.SetProcess(cmd.Process)

		// write parent pid
		if self.Pid.Parent != "" {
			if err := self.WritePid(self.Pid.Parent, os.Getpid()); err != nil {
				log.Print(err)
			}
		}

		// write child pid
		if self.Pid.Child != "" {
			if err := self.WritePid(self.Pid.Child, self.process.GetPid()); err != nil {
				log.Print(err)
			}
		}

		self.Control.state <- cmd.Wait()
		return
	}()
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
		process: &Process{&os.Process{}},
	}, nil
}
