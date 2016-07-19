package immortal

import (
	"gopkg.in/yaml.v2"
	"io/ioutil"
	"log"
	"os"
	"os/user"
	"path/filepath"
)

type Daemon struct {
	command []string
	count   int64
	owner   *user.User
	pid     int
	sdir    string
	ctrl    Ctrl
	logger  *log.Logger
	run     Run
}

type Run struct {
	Command   string
	Ctrl      bool
	Cwd       string
	Env       map[string]string
	Logfile   string
	Logger    string
	Signals   map[string]string
	User      string
	ParentPid string
	ChildPid  string
	FollowPid string
}

type Ctrl struct {
	fifo   chan Return
	quit   chan struct{}
	state  chan error
	status *os.File
}

type Return struct {
	err error
	msg string
}

// New return a instances of Daemon
//      u - usr
//      c - config
//      d - working dir
//      f - follow pid
//      l - log file
// logger - command to pipe stdout/stderr
//      P - parent pidfile
//      p - child pidfile
//    cmd - command to supervice
//   ctrl - create supervise dir
func New(u *user.User, c, d, f, l, logger, P, p *string, cmd []string, ctrl *bool) (*Daemon, error) {
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

	var sdir string
	if *ctrl {
		wd, err := os.Getwd()
		if err != nil {
			return nil, err
		}
		sdir = filepath.Join(wd, "supervise")
		if err := os.MkdirAll(sdir, 0700); err != nil {
			return nil, err
		}
	}

	return &Daemon{
		owner:   u,
		command: cmd,
		sdir:    sdir,
		run: Run{
			Cwd:       *d,
			FollowPid: *f,
			Logfile:   *l,
			Logger:    *logger,
			ParentPid: *P,
			ChildPid:  *p,
			Ctrl:      *ctrl,
		},
		ctrl: Ctrl{
			fifo:  make(chan Return),
			quit:  make(chan struct{}),
			state: make(chan error),
		},
	}, nil
}
