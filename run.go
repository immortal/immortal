package immortal

import (
	"bufio"
	"bytes"
	"io"
	"os/exec"
	"strconv"
	"syscall"
)

func (self *Daemon) Run(args []string) error {
	cmd := exec.Command(args[0], args[1:]...)

	sysProcAttr := new(syscall.SysProcAttr)
	if self.owner != nil {
		uid, err := strconv.Atoi(self.owner.Uid)
		if err != nil {
			return err
		}

		gid, err := strconv.Atoi(self.owner.Gid)
		if err != nil {
			return err
		}

		//	https://golang.org/pkg/syscall/#SysProcAttr
		sysProcAttr.Credential = &syscall.Credential{
			Uid: uint32(uid),
			Gid: uint32(gid),
		}
	}

	cmd.SysProcAttr = sysProcAttr

	stdout := new(bytes.Buffer)
	stderr := new(bytes.Buffer)

	cmd.Stdout = stdout
	cmd.Stderr = stderr

	if err := cmd.Start(); err != nil {
		return err
	}

	Log(cmd.Process.Pid)

	in := bufio.NewScanner(io.MultiReader(stdout, stderr))
	for in.Scan() {
		Log(in.Text())
	}

	if err := cmd.Wait(); err != nil {
		return err
	}

	return nil
}
