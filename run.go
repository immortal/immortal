package immortal

import (
	"bufio"
	"io"
	"os/exec"
	"strconv"
	"syscall"
)

func (self *Daemon) stdHandler(p io.ReadCloser) {
	in := bufio.NewScanner(p)
	for in.Scan() {
		Log(in.Text())
	}
}

func (self *Daemon) Run(args []string, ch chan<- error) {
	cmd := exec.Command(args[0], args[1:]...)

	sysProcAttr := new(syscall.SysProcAttr)
	if self.owner != nil {
		uid, err := strconv.Atoi(self.owner.Uid)
		if err != nil {
			ch <- err
			return
		}

		gid, err := strconv.Atoi(self.owner.Gid)
		if err != nil {
			ch <- err
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
		ch <- err
		return
	}

	stderr, err := cmd.StderrPipe()
	if err != nil {
		ch <- err
		return
	}

	if err := cmd.Start(); err != nil {
		ch <- err
		return
	}

	go self.stdHandler(stdout)
	go self.stdHandler(stderr)

	ch <- cmd.Wait()
}
