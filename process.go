package immortal

import (
	"fmt"
	"io"
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
	Start() (*process, error)
}

type process struct {
	*Config
	Logger
	cmd   *exec.Cmd
	eTime time.Time
	errch chan error
	quit  chan struct{}
	sTime time.Time
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
		r, w = io.Pipe()
		p.cmd.Stdout = w
		p.cmd.Stderr = w
		go p.Logger.Log(r)
	} else {
		p.cmd.Stdin = nil
		p.cmd.Stdout = nil
		p.cmd.Stderr = nil
	}

	// Start the process
	if err := p.cmd.Start(); err != nil {
		return nil, err
	}
	p.sTime = time.Now()

	p.errch = make(chan error)
	go func(w *io.PipeWriter) {
		err := p.cmd.Wait()
		p.eTime = time.Now()
		if w != nil {
			w.Close()
			close(p.quit)
		}
		p.errch <- err
	}(w)
	return p, nil
}

// Kill the entire Process group.
func (p *process) Kill() error {
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

// Signal sends a signal to the Process
func (p *process) Signal(sig os.Signal) error {
	return p.cmd.Process.Signal(sig)
}

func NewProcess(cfg *Config) *process {
	quit := make(chan struct{})
	return &process{
		Config: cfg,
		Logger: &LogWriter{
			logger: NewLogger(cfg, quit),
		},
		quit: quit,
	}
}
