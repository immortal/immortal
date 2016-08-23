package immortal

import (
	"fmt"
	"io"
	"os"
	"os/exec"
	"strconv"
	"syscall"
)

// Proc implements the Process interface
type Proc struct {
	*Config
	Logger
}

// exec runs the command
func (p *Proc) Start() (*exec.Cmd, *io.PipeWriter, error) {
	cmd := exec.Command(p.command[0], p.command[1:]...)

	// change working directory
	if p.Cwd != "" {
		cmd.Dir = p.Cwd
	}

	// set environment vars
	if p.Env != nil {
		env := os.Environ()
		for k, v := range p.Env {
			env = append(env, fmt.Sprintf("%s=%s", k, v))
		}
		cmd.Env = env
	}

	sysProcAttr := new(syscall.SysProcAttr)

	// set owner
	if p.user != nil {
		uid, err := strconv.Atoi(p.user.Uid)
		if err != nil {
			return nil, nil, err
		}

		gid, err := strconv.Atoi(p.user.Gid)
		if err != nil {
			return nil, nil, err
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
	cmd.SysProcAttr = sysProcAttr

	// log only if are available loggers
	var (
		r *io.PipeReader
		w *io.PipeWriter
	)
	if p.Logger.IsLogging() {
		//defer func() {
		//w.Close()
		//}()
		r, w = io.Pipe()
		cmd.Stdout = w
		cmd.Stderr = w
		go p.Logger.StdHandler(r)
	} else {
		cmd.Stdin = nil
		cmd.Stdout = nil
		cmd.Stderr = nil
	}

	if err := cmd.Start(); err != nil {
		return nil, w, err
	}

	return cmd, w, nil
}
