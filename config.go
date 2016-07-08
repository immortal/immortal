package immortal

import (
	"gopkg.in/yaml.v2"
	"io/ioutil"
	"os/user"
)

type Daemon struct {
	owner   *user.User
	User    string
	Command string
	command []string
	Pidfile string
	Log     string
	Env     map[string]string
	Cwd     string
	signals map[string]string
	err     chan error
	monitor chan struct{}
	status  chan error
	pid     int
}

func New(u *user.User, c, p, l *string, cmd []string) (*Daemon, error) {
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
		Log:     *l,
		command: cmd,
		err:     make(chan error, 1),
		status:  make(chan error, 1),
		monitor: make(chan struct{}),
	}, nil
}
