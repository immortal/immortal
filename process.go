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
	Start() (*process, error)
	Pid() int
}

type process struct {
	*Config
	Logger
	cmd   *exec.Cmd
	eTime time.Time
	sTime time.Time
	err   chan error
}

// Start runs the command
func (p *process) Start() (*process, error) {
	p.cmd = exec.Command(p.command[0], p.command[1:]...)

	// change working directory
	if p.Cwd != "" {
		p.cmd.Dir = p.Cwd
	}

	// set environment vars
	if p.Env != nil {
		env := os.Environ()
		for k, v := range p.Env {
			env = append(env, fmt.Sprintf("%s=%s", k, v))
		}
		p.cmd.Env = env
	}

	sysProcAttr := new(syscall.SysProcAttr)

	// set owner
	if p.user != nil {
		uid, err := strconv.Atoi(p.user.Uid)
		if err != nil {
			return nil, err
		}

		gid, err := strconv.Atoi(p.user.Gid)
		if err != nil {
			return nil, err
		}

		sysProcAttr.Credential = &syscall.Credential{
			Uid: uint32(uid),
			Gid: uint32(gid),
		}
	}

	// Set Process group ID to Pgid, or, if Pgid == 0, to new pid
	sysProcAttr.Setpgid = true
	sysProcAttr.Pgid = 0

	// set the attributes
	p.cmd.SysProcAttr = sysProcAttr

	// log only if are available loggers
	var (
		r *io.PipeReader
		w *io.PipeWriter
	)
	if p.Logger.IsLogging() {
		defer func() {
			w.Close()
		}()
		r, w = io.Pipe()
		p.cmd.Stdout = w
		p.cmd.Stderr = w
		go p.Logger.StdHandler(r)
	} else {
		p.cmd.Stdin = nil
		p.cmd.Stdout = nil
		p.cmd.Stderr = nil
	}

	if err := p.cmd.Start(); err != nil {
		return nil, err
	}
	p.sTime = time.Now()

	p.err = make(chan error)
	go func() {
		err := p.cmd.Wait()
		p.eTime = time.Now()
		p.err <- err
	}()
	return p, nil
}

func (p *process) Kill() error {
	// to kill the entire Process group.
	processGroup := 0 - p.cmd.Process.Pid
	return syscall.Kill(processGroup, syscall.SIGKILL)
}

// Pid return Process PID
func (p *process) Pid() int {
	if p.cmd == nil || p.cmd.Process == nil {
		return 0
	}
	return p.cmd.Process.Pid
}

// WritePid write pid to file
func (self *process) WritePid(file string, pid int) error {
	if err := ioutil.WriteFile(file, []byte(fmt.Sprintf("%d", pid)), 0644); err != nil {
		return err
	}
	return nil
}

func (self *process) Signal(sig os.Signal) error {
	return self.cmd.Process.Signal(sig)
}

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
func NewProcess(cfg *Config) *process {
	return &process{
		Config: cfg,
		Logger: &LogWriter{
			logger: NewLogger(cfg),
		},
	}
}
