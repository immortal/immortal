package immortal

import (
	"bufio"
	"fmt"
	"io"
	"log"
	"os/exec"
	"strconv"
	"sync/atomic"
	"syscall"
)

func (self *Daemon) stdHandler(p io.ReadCloser) {
	in := bufio.NewScanner(p)
	for in.Scan() {
		self.Log(in.Text())
	}
	p.Close()
}

func (self *Daemon) Run(ch chan<- error) {
	atomic.AddInt64(&self.count, 1)
	log.Print(Green(fmt.Sprintf("count: %v", self.count)))

	cmd := exec.Command(self.command[0], self.command[1:]...)

	sysProcAttr := new(syscall.SysProcAttr)

	// set owner
	if self.owner != nil {
		uid, err := strconv.Atoi(self.owner.Uid)
		if err != nil {
			ch <- err
		}

		gid, err := strconv.Atoi(self.owner.Gid)
		if err != nil {
			ch <- err
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

	r, w := io.Pipe()
	cmd.Stdout = w
	cmd.Stderr = w

	go self.stdHandler(r)

	go func() {
		defer w.Close()

		if err := cmd.Start(); err != nil {
			ch <- err
		}

		self.pid = cmd.Process.Pid

		// follow pid
		if self.run.FollowPid != "" {
			go self.watchPid(self.ctrl.state)
		}

		ch <- cmd.Wait()
	}()
}
