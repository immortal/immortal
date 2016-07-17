package immortal

import (
	"github.com/immortal/logrotate"
	"github.com/immortal/multiwriter"
	"gopkg.in/yaml.v2"
	"io"
	"io/ioutil"
	"log"
	"os"
	"os/exec"
	"os/user"
	"path/filepath"
	"strings"
	"time"
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

func (self *Daemon) Logger() {
	var (
		ch    chan error
		err   error
		multi io.Writer
		file  io.WriteCloser
		w     io.WriteCloser
	)

	ch = make(chan error)

	if self.run.Logfile != "" {
		//file, err = os.OpenFile(self.run.Logfile, os.O_CREATE|os.O_WRONLY|os.O_APPEND, 0600)
		file, err = logrotate.New(self.run.Logfile)
		if err != nil {
			log.Printf("Failed to open log file %q: %s\n", self.run.Logfile, err)
			return
		}
	}

	runLogger := func() {
		command := strings.Fields(self.run.Logger)
		cmd := exec.Command(command[0], command[1:]...)
		w, err = cmd.StdinPipe()
		if err != nil {
			log.Printf("logger PIPE error: %s", err)
			ch <- err
			return
		}
		go func() {
			if err := cmd.Start(); err != nil {
				ch <- err
			}
			ch <- cmd.Wait()
		}()
	}

	if self.run.Logger != "" {
		runLogger()

		go func() {
			for {
				select {
				case err = <-ch:
					log.Print("logger exited ", err.Error())
					time.Sleep(time.Second)
					runLogger()
					multi = multiwriter.New(file, w)
					self.logger = log.New(multi, "", 0)
				}
			}
		}()
		multi = multiwriter.New(file, w)
	} else {
		multi = multiwriter.New(file)
	}

	// create the logger
	self.logger = log.New(multi, "", 0)
}
