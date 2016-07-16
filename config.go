package immortal

import (
	"gopkg.in/yaml.v2"
	"io"
	"io/ioutil"
	"log"
	"os"
	"os/exec"
	"os/user"
	"path/filepath"
)

type Daemon struct {
	owner   *user.User
	command []string
	pid     int
	run     Run
	count   int64
	sdir    string
	ctrl    Ctrl
	logger  *log.Logger
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
	state  chan error
	fifo   chan Return
	quit   chan struct{}
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
			state: make(chan error),
			fifo:  make(chan Return),
			quit:  make(chan struct{}),
		},
	}, nil
}

func (self *Daemon) Init() {
	if self.run.Logfile != "" {
		file, err := os.OpenFile(self.run.Logfile, os.O_CREATE|os.O_WRONLY|os.O_APPEND, 0600)
		if err != nil {
			log.Printf("Failed to open log file %q: %s\n", self.run.Logfile, err)
			return
		}
		logger := exec.Command("logger", "-t", "immortal-multiwriter")
		w, _ := logger.StdinPipe()
		go func() {
			defer w.Close()
			logger.Start()
			logger.Wait()
		}()
		multi := io.MultiWriter(file, w)
		self.logger = log.New(multi, "", 0)
	}
}
