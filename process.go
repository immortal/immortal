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

// Process interface
type Process interface {
	Kill() error
	Pid() int
	Signal(sig os.Signal) error
	Start(cfg *Config) (*exec.Cmd, error)
	Stop() error
	Uptime() time.Duration
}

func NewProcess(cfg *Config) Process {
	p := &proc{
		Logger: &LogWriter{
			logger: NewLogger(cfg),
		},
	}
	return p
}

// proc implements the Process interface
type proc struct {
	Logger
	cmd   *exec.Cmd
	eTime time.Time
	sTime time.Time
}

func (self *proc) Kill() error {
	// to kill the entire process group.
	processGroup := 0 - self.cmd.Process.Pid
	return syscall.Kill(processGroup, syscall.SIGKILL)
}

// Pid return process PID
func (self *proc) Pid() int {
	if self.cmd == nil || self.cmd.Process == nil {
		return 0
	}
	return self.cmd.Process.Pid
}

// Signal sends a signal to the process
func (self *proc) Signal(sig os.Signal) error {
	return self.cmd.Process.Signal(sig)
}

// exec runs the command
func (self *proc) Start(cfg *Config) (*exec.Cmd, error) {
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
			return nil, err
		}

		gid, err := strconv.Atoi(cfg.user.Gid)
		if err != nil {
			return nil, err
		}

		// https://golang.org/pkg/syscall/#SysprocAttr
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
		return nil, err
	}

	// sTime start time
	self.sTime = time.Now()

	return self.cmd, nil

	/// move this out
	//
	//// write parent pid
	//if cfg.Pid.Parent != "" {
	//if err := self.WritePid(cfg.Pid.Parent, os.Getpid()); err != nil {
	//return err
	//}
	//}

	//// write child pid
	//if cfg.Pid.Child != "" {
	//if err := self.WritePid(cfg.Pid.Child, self.cmd.Process.Pid); err != nil {
	//return err
	//}
	//}
}

func (self *proc) Stop() error {
	return nil
}
func (self *proc) Uptime() time.Duration {
	return time.Since(self.sTime)
}

// WritePid write pid to file
func (self *proc) WritePid(file string, pid int) error {
	if err := ioutil.WriteFile(file, []byte(fmt.Sprintf("%d", pid)), 0644); err != nil {
		return err
	}
	return nil
}
