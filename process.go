package immortal

import (
	"fmt"
	"io"
	"io/ioutil"
	"os"
	"os/exec"
	"strconv"
	"syscall"
	"time"
)

type Process interface {
	Exec(cfg *Config, r chan<- struct{}) error
	Kill() error
	Pid() int
	Signal(sig os.Signal) error
	Stop() error
	Uptime() time.Duration
}

func NewProcess(cfg *Config) Process {
	p := &Proc{
		Logger: &LogWriter{
			logger: NewLogger(cfg),
		},
	}
	return p
}

type Proc struct {
	cmd *exec.Cmd
	Logger
	start time.Time
}

func (self *Proc) Kill() error {
	// to kill the entire process group.
	processGroup := 0 - self.cmd.Process.Pid
	return syscall.Kill(processGroup, syscall.SIGKILL)
}

// Pid return process PID
func (self *Proc) Pid() int {
	return self.cmd.Process.Pid
}

// Signal sends a signal to the Process
func (self *Proc) Signal(sig os.Signal) error {
	return self.cmd.Process.Signal(sig)
}

// exec runs the command
func (self *Proc) Exec(cfg *Config, running chan<- struct{}) error {
	self.cmd = exec.Command(cfg.command[0], cfg.command[1:]...)

	// change working directory
	if cfg.Cwd != "" {
		self.cmd.Dir = cfg.Cwd
	}

	// set environment vars
	if cfg.Env != nil {
		env := os.Environ()
		for k, v := range cfg.Env {
			env = append(env, fmt.Sprintf("%s=%s", k, v))
		}
		self.cmd.Env = env
	}

	sysProcAttr := new(syscall.SysProcAttr)

	// set owner
	if cfg.user != nil {
		uid, err := strconv.Atoi(cfg.user.Uid)
		if err != nil {
			return err
		}

		gid, err := strconv.Atoi(cfg.user.Gid)
		if err != nil {
			return err
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
		defer func() {
			w.Close()
		}()
		r, w = io.Pipe()
		self.cmd.Stdout = w
		self.cmd.Stderr = w
		go self.Logger.StdHandler(r)
	} else {
		self.cmd.Stdin = nil
		self.cmd.Stdout = nil
		self.cmd.Stderr = nil
	}

	if err := self.cmd.Start(); err != nil {
		return err
	}

	// set start time
	self.start = time.Now()

	// write parent pid
	if cfg.Pid.Parent != "" {
		if err := self.WritePid(cfg.Pid.Parent, os.Getpid()); err != nil {
			return err
		}
	}

	// write child pid
	if cfg.Pid.Child != "" {
		if err := self.WritePid(cfg.Pid.Child, self.cmd.Process.Pid); err != nil {
			return err
		}
	}

	// after writing the pids confirm
	running <- struct{}{}

	// return after process has finished
	return self.cmd.Wait()
}

func (self *Proc) Stop() error {
	return nil
}
func (self *Proc) Uptime() time.Duration {
	return time.Since(self.start)
}

// WritePid write pid to file
func (self *Proc) WritePid(file string, pid int) error {
	if err := ioutil.WriteFile(file, []byte(fmt.Sprintf("%d", pid)), 0644); err != nil {
		return err
	}
	return nil
}
