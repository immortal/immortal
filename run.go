package immortal

import (
	"bufio"
	"fmt"
	"io"
	//	"os"
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
	Log(Green(fmt.Sprintf("count: %v", self.count)))

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

	//file, err := os.Create("/tmp/tmp.log")
	//if err != nil {
	//self.ctrl.err <- err
	//}
	//defer file.Close()
	//cmd.Stdout = file
	//cmd.Stderr = file

	_, err := cmd.StdoutPipe()
	if err != nil {
		return
	}
	_, err = cmd.StderrPipe()
	if err != nil {
		return
	}

	if err := cmd.Start(); err != nil {
		self.ctrl.err <- err
		return
	}

	self.pid = cmd.Process.Pid

	self.ctrl.state <- cmd.Wait()
}
