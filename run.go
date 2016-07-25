package immortal

import (
	"bufio"
	"io"
	"log"
	"os"
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
	if atomic.SwapUint32(&self.count, uint32(1)) != 0 {
		log.Printf("PID: %d running", self.process.Pid)
		return
	}

	cmd := exec.Command(self.command[0], self.command[1:]...)

	if self.run.Cwd != "" {
		cmd.Dir = self.run.Cwd
	}

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

	var (
		r *io.PipeReader
		w *io.PipeWriter
	)
	if self.log {
		r, w = io.Pipe()
		cmd.Stdout = w
		cmd.Stderr = w
		go self.stdHandler(r)
	} else {
		cmd.Stdin = nil
		cmd.Stdout = nil
		cmd.Stderr = nil
	}

	go func() {
		// count_defer defaults to 0
		// 1 to run only once (don't restart)
		defer atomic.StoreUint32(&self.count, self.count_defer)

		if self.log {
			defer w.Close()
		}

		if err := cmd.Start(); err != nil {
			ch <- err
		}

		self.process = cmd.Process

		// follow pid
		if self.run.FollowPid != "" {
			go self.watchPid(self.process.Pid, self.ctrl.state)
		}

		// write parent pid
		if self.run.ParentPid != "" {
			if err := WritePid(self.run.ParentPid, os.Getpid()); err != nil {
				log.Print(err)
			}
		}

		// write child pid
		if self.run.ChildPid != "" {
			if err := WritePid(self.run.ChildPid, self.process.Pid); err != nil {
				log.Print(err)
			}
		}

		ch <- cmd.Wait()
	}()
}
