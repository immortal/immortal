package immortal

import (
	"fmt"
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
	Signal(sig syscall.Signal) error
	Start() (*process, error)
}

type process struct {
	*Config
	Logger
	cmd          *exec.Cmd
	errch        chan error
	quit         chan struct{}
	sTime, eTime time.Time
}

// Start runs the command
func (p *process) Start() (*process, error) {
	// command obtained from Config parent
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
		pr, pw *os.File
		e      error
	)
	if p.Logger.IsLogging() {
		// create the pipes
		pr, pw, e = os.Pipe()
		if e == nil {
			p.cmd.Stdout = pw
			p.cmd.Stderr = pw
			go p.Logger.Log(pr)
		}
	}

	// Start the process
	if err := p.cmd.Start(); err != nil {
		return nil, err
	}

	// set start time
	p.sTime = time.Now()

	// create error channel
	p.errch = make(chan error, 1)

	// wait process to finish
	go func(w *os.File) {
		err := p.cmd.Wait()
		if w != nil {
			w.Close()
			close(p.quit)
		}
		p.errch <- err
	}(pw)

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
func (p *process) Signal(sig syscall.Signal) error {
	return syscall.Kill(p.cmd.Process.Pid, sig)
}

// NewProcess return process instance
func NewProcess(cfg *Config) *process {
	qch := make(chan struct{})
	return &process{
		Config: cfg,
		Logger: &LogWriter{
			logger: NewLogger(cfg, qch),
		},
		quit: qch,
	}
}
