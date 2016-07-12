package immortal

import (
	"bufio"
	"io"
	"os/exec"
	"strconv"
	"sync/atomic"
	"syscall"
)

func (self *Daemon) stdHandler(p io.ReadCloser, e bool) {
	in := bufio.NewScanner(p)
	for in.Scan() {
		if e {
			Log(Red(in.Text()))
		} else {
			Log(in.Text())
		}
	}
}

func (self *Daemon) Run() {
	atomic.AddInt64(&self.count, 1)

	cmd := exec.Command(self.command[0], self.command[1:]...)

	sysProcAttr := new(syscall.SysProcAttr)
	// set owner
	if self.owner != nil {
		uid, err := strconv.Atoi(self.owner.Uid)
		if err != nil {
			self.ctrl.err <- err
			return
		}

		gid, err := strconv.Atoi(self.owner.Gid)
		if err != nil {
			self.ctrl.err <- err
			return
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

	stdout, err := cmd.StdoutPipe()
	if err != nil {
		self.ctrl.err <- err
		return
	}

	stderr, err := cmd.StderrPipe()
	if err != nil {
		self.ctrl.err <- err
		return
	}

	go self.stdHandler(stdout, false)
	go self.stdHandler(stderr, true)

	if err := cmd.Start(); err != nil {
		self.ctrl.err <- err
		return
	}

	self.pid = cmd.Process.Pid

	self.ctrl.state <- cmd.Wait()
}
