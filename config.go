package immortal

import (
	"gopkg.in/yaml.v2"
	"io/ioutil"
	"os"
	"os/user"
)

type Daemon struct {
	owner   *user.User
	command []string
	pid     int
	err     chan error
	state   chan error
	run     Run
	count   int64
	sdir    string
}

type Run struct {
	Command string
	Cwd     string
	Env     map[string]string
	Log     string
	Pidfile string
	Signals map[string]string
	User    string
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

	wd, err := os.Getwd()
	if err != nil {
		return nil, err
	}
	if err := os.Mkdir(wd+"/supervise", 0700); err != nil {
		if os.IsNotExist(err) {
			return nil, err
		}
	}

	return &Daemon{
		owner: u,
		sdir:  wd + "/supervise",
		run: Run{
			Pidfile: *p,
			Log:     *l,
		},
		command: cmd,
		err:     make(chan error, 1),
		state:   make(chan error, 1),
	}, nil
}
