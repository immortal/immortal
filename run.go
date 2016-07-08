package immortal

import (
	"bufio"
	"io"
	"os/exec"
	"strconv"
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
	cmd := exec.Command(self.command[0], self.command[1:]...)

	sysProcAttr := new(syscall.SysProcAttr)
	if self.owner != nil {
		uid, err := strconv.Atoi(self.owner.Uid)
		if err != nil {
			self.status <- err
			return
		}

		gid, err := strconv.Atoi(self.owner.Gid)
		if err != nil {
			self.status <- err
			return
		}

		//	https://golang.org/pkg/syscall/#SysProcAttr
		sysProcAttr.Credential = &syscall.Credential{
			Uid: uint32(uid),
			Gid: uint32(gid),
		}
	}

	cmd.SysProcAttr = sysProcAttr

	stdout, err := cmd.StdoutPipe()
	if err != nil {
		self.status <- err
		return
	}

	stderr, err := cmd.StderrPipe()
	if err != nil {
		self.status <- err
		return
	}

	if err := cmd.Start(); err != nil {
		self.status <- err
		return
	}

	go self.stdHandler(stdout, false)
	go self.stdHandler(stderr, true)

	self.Pid <- cmd.Process.Pid
	self.status <- cmd.Wait()
}
