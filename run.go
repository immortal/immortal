package immortal

import (
	"bufio"
	"bytes"
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

	//	var stdout bytes.Buffer
	stdout := new(bytes.Buffer)
	stderr := new(bytes.Buffer)

	cmd.Stdout = stdout
	cmd.Stderr = stderr

	if err := cmd.Start(); err != nil {
		return err
	}

	if err := cmd.Wait(); err != nil {
		return err
	}

	// 	in := bufio.NewScanner(io.MultiReader(stdout, stderr))

	in_out := bufio.NewScanner(stdout)
	for in_out.Scan() {
		Log(in_out.Text())
	}

	in_err := bufio.NewScanner(stderr)
	for in_err.Scan() {
		Log(Red(in_err.Text()))
	}

	return nil
}
