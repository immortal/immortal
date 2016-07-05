package immortal

import (
	"bufio"
	"fmt"
	"io"
	"os/exec"
	"os/user"
	"strconv"
	"syscall"
)

type Daemon struct {
	owner  *user.User
	follow string
	quiet  bool
}

func NewDaemon(u, f *string, q *bool) (*Daemon, error) {
	if *u == "" {
		return &Daemon{nil, *f, *q}, nil
	}
	// check if user exist and if not exit
	usr, err := user.Lookup(*u)
	if err != nil {
		if _, ok := err.(user.UnknownUserError); ok {
			return nil, fmt.Errorf("User %s does not exist.", *u)
		} else if err != nil {
			return nil, fmt.Errorf("Error looking up user: %s", *u)
		}
	}
	return &Daemon{
		owner:  usr,
		follow: *f,
		quiet:  *q,
	}, nil
}

func (self *Daemon) Run(args []string) error {
	cmd := exec.Command(args[0], args[1:]...)
	stdout, err := cmd.StdoutPipe()
	if err != nil {
		return err
	}
	stderr, err := cmd.StderrPipe()
	if err != nil {
		return err
	}

	if self.owner != nil {
		uid, err := strconv.Atoi(self.owner.Uid)
		if err != nil {
			return err
		}

		gid, err := strconv.Atoi(self.owner.Gid)
		if err != nil {
			return err
		}

		cmd.SysProcAttr = &syscall.SysProcAttr{
			Credential: &syscall.Credential{
				Uid: uint32(uid),
				Gid: uint32(gid),
			},
		}
	}

	if err := cmd.Start(); err != nil {
		return err
	}

	// write each line to your log, or anything you need
	in := bufio.NewScanner(io.MultiReader(stdout, stderr))
	for in.Scan() {
		Log(in.Text())
	}

	if err := cmd.Wait(); err != nil {
		return err
	}

	return nil
}
