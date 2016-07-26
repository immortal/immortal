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
	command     []string
	count       uint32
	count_defer uint32
	ctrl        Ctrl
	log         bool
	logger      *log.Logger
	owner       *user.User
	process     *os.Process
	run         Run
}

type Run struct {
	Command   string
	Ctrl      bool
	Cwd       string
	Env       map[string]string
	Logfile   string
	Logger    string
	User      string
	ParentPid string
	ChildPid  string
	FollowPid string
}

type Ctrl struct {
	fifo         chan Return
	quit         chan struct{}
	state        chan error
	control_fifo *os.File
	status_fifo  *os.File
}

type Return struct {
	err error
	msg string
}

// New return a instances of Daemon
//      u - usr
//      c - config
//      d - working dir
//      e - envdir
//      f - follow pid
//      l - log file
// logger - command to pipe stdout/stderr
//      P - parent pidfile
//      p - child pidfile
//    cmd - command to supervise
//   ctrl - create supervise dir
func New(u *user.User, c, d, e, f, l, logger, p, P *string, cmd []string, ctrl *bool) (*Daemon, error) {
	var (
		env map[string]string
		err error
	)

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

	// set environment
	if *e != "" {
		env, err = GetEnv(*e)
		if err != nil {
			return nil, err
		}
	}

	daemon := &Daemon{
		owner:   u,
		command: cmd,
		run: Run{
			Cwd:       *d,
			Env:       env,
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
	}

	if *ctrl {
		wd, err := os.Getwd()
		if err != nil {
			return nil, err
		}
		wd = filepath.Join(wd, "supervise")
		if err := os.MkdirAll(wd, 0700); err != nil {
			return nil, err
		}
		// create control pipe
		daemon.ctrl.control_fifo, err = MakeFIFO(filepath.Join(wd, "control"))
		if err != nil {
			return nil, err
		}
		// create status pipe
		daemon.ctrl.status_fifo, err = MakeFIFO(filepath.Join(wd, "ok"))
		if err != nil {
			return nil, err
		}
		// create lock
		if err = Lock(filepath.Join(wd, "lock")); err != nil {
			return nil, err
		}
	}

	return daemon, nil
}
