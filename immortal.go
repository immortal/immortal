package immortal

import (
	"bufio"
	"gopkg.in/yaml.v2"
	"io"
	"io/ioutil"
	"os/exec"
	"os/user"
	"strconv"
	"syscall"
)

type Daemon struct {
	owner   *user.User
	Pidfile string
	Quiet   bool
	Env     map[string]string
	Cmd     string
	Cwd     string
	signals map[string]string
}

func New(u *user.User, c, p *string, q *bool) (*Daemon, error) {
	if *c != "" {
		yml_file, err := ioutil.ReadFile(*c)
		if err != nil {
			return nil, err
		}

		var D Daemon

		if err := yaml.Unmarshal(yml_file, &D); err != nil {
			return nil, err
		}

		return &D, nil
	}
	return &Daemon{
		owner:   u,
		Pidfile: *p,
		Quiet:   *q,
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
