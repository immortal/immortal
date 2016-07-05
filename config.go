package immortal

import (
	"gopkg.in/yaml.v2"
	"io/ioutil"
	"os/user"
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
