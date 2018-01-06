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
	GetProcess() *process
}

type process struct {
	*Config
	Logger
	LoggerStderr Logger
	cmd          *exec.Cmd
	errch        chan error
	quit         chan struct{}
	sTime, eTime time.Time
}

// SetEnv set environment variables - If the Cmd.Env contains duplicate
// environment keys, only the last value in the slice for each duplicate
// key is used.
func (p *process) SetEnv(env []string) {
	if p.Env != nil {
		for k, v := range p.Env {
			env = append(env, fmt.Sprintf("%s=%s", k, v))
		}
		p.cmd.Env = env
	}
}

// SetsysProcAttr - set process group ID and owner (run on behalf)
func (p *process) SetsysProcAttr() error {
	sysProcAttr := &syscall.SysProcAttr{
		Setpgid: true, // Set process group ID to Pgid, or, if Pgid == 0, to new pid.
		Pgid:    0,    // Child's process group ID if Setpgid.
	}

	// set owner
	if p.user != nil {
		uid, err := strconv.Atoi(p.user.Uid)
		if err != nil {
			return err
		}
		gid, err := strconv.Atoi(p.user.Gid)
		if err != nil {
			return err
		}
		sysProcAttr.Credential = &syscall.Credential{
			Uid: uint32(uid),
			Gid: uint32(gid),
		}
	}

	// set the attributes
	p.cmd.SysProcAttr = sysProcAttr

	return nil
}

// Start runs the command
func (p *process) Start() (*process, error) {
	// command obtained from Config parent
	p.cmd = exec.Command(p.command[0], p.command[1:]...)

	// change working directory
	if p.Cwd != "" {
		p.cmd.Dir = p.Cwd
	}

	// set environment variables
	p.SetEnv(os.Environ())

	// set sysProcAttr
	if err := p.SetsysProcAttr(); err != nil {
		return nil, err
	}

	var (
		prStdout, prStderr, pwStdout, pwStderr *os.File
		e                                      error
	)
	// log only if are available loggers
	if p.Logger.IsLogging() && p.LoggerStderr.IsLogging() {
		// create the pipes for Stdout
		prStdout, pwStdout, e = os.Pipe()
		if e == nil {
			p.cmd.Stdout = pwStdout
			go p.Logger.Log(prStdout)
		}
		prStderr, pwStderr, e = os.Pipe()
		if e == nil {
			p.cmd.Stderr = pwStderr
			go p.LoggerStderr.Log(prStderr)
		}
	} else if p.Logger.IsLogging() {
		// create the pipes for Stdout
		prStdout, pwStdout, e = os.Pipe()
		if e == nil {
			p.cmd.Stdout = pwStdout
			p.cmd.Stderr = pwStdout
			go p.Logger.Log(prStdout)
		}
	} else if p.LoggerStderr.IsLogging() {
		// create the pipes for Stdout
		prStderr, pwStderr, e = os.Pipe()
		if e == nil {
			p.cmd.Stderr = pwStderr
			go p.LoggerStderr.Log(prStderr)
		}
	}

	// Start the process
	if err := p.cmd.Start(); err != nil {
		return nil, err
	}

	// set start time
	p.sTime = time.Now()

	// wait process to finish in a goroutine
	go p.Wait(pwStdout, pwStderr)

	return p, nil
}

// Wait - wait process to finish
func (p *process) Wait(stdout, stderr *os.File) {
	err := p.cmd.Wait()
	if stdout != nil {
		stdout.Close()
		close(p.quit)
	}
	if stderr != nil {
		stderr.Close()
	}
	p.errch <- err
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

// GetProccess
func (p *process) GetProcess() *process {
	return p
}

// NewProcess return process instance
func NewProcess(cfg *Config) *process {
	qch := make(chan struct{})
	return &process{
		Config: cfg,
		Logger: &LogWriter{
			logger: NewLogger(cfg, qch),
		},
		LoggerStderr: &LogWriter{
			logger: NewStderrLogger(cfg),
		},
		errch: make(chan error, 1),
		quit:  qch,
		sTime: time.Now(),
	}
}
