package immortal

import (
	"flag"
	"fmt"
	"gopkg.in/yaml.v2"
	"io/ioutil"
	"os"
	"os/user"
	"path/filepath"
)

type Config struct {
	Ctrl       bool
	Version    bool
	Configfile string
	Wrkdir     string
	Envdir     string
	FollowPid  string
	Logfile    string
	Logger     string
	ChildPid   string
	ParentPid  string
	User       string
	Command    string
	Configuration
}

func NewConfig() *Config {
	return &Config{
		Configuration: &Config{},
	}
}

func (self *Config) Exists(path string) bool {
	if _, err := os.Stat(path); os.IsNotExist(err) {
		return false
	}
	return true
}

func (self *Config) IsExec(path string) (bool, error) {
	if f, err := os.Stat(path); err != nil {
		if os.IsNotExist(err) {
			return false, nil
		} else {
			return false, err
		}
	} else if m := f.Mode(); !m.IsDir() && m&0111 != 0 {
		return true, nil
	} else {
		return false, nil
	}
}

func (self *Config) Parse() *Config {
	self.ParseArgs(flag.CommandLine)
	flag.CommandLine.Parse(os.Args[1:])
	return self
}

func (self *Config) ParseArgs(f *flag.FlagSet) {
	f.BoolVar(&self.Ctrl, "ctrl", false, "Create supervise directory")
	f.BoolVar(&self.Version, "v", false, "Print version")
	f.StringVar(&self.Configfile, "c", "", "`run.yml` configuration file")
	f.StringVar(&self.Wrkdir, "d", "", "Change to `dir` before starting the command")
	f.StringVar(&self.Envdir, "e", "", "Set environment variables specified by files in the `dir`")
	f.StringVar(&self.FollowPid, "f", "", "Follow PID in `pidfile`")
	f.StringVar(&self.Logfile, "l", "", "Write stdout/stderr to `logfile`")
	f.StringVar(&self.Logger, "logger", "", "A `command` to pipe stdout/stderr to stdin")
	f.StringVar(&self.ChildPid, "p", "", "Path to write the child `pidfile`")
	f.StringVar(&self.ParentPid, "P", "", "Path to write the supervisor `pidfile`")
	f.StringVar(&self.User, "u", "", "Execute command on behalf `user`")

	f.Usage = func() {
		fmt.Fprintf(os.Stderr, "usage: %s [-v -ctrl] [-d dir] [-e dir] [-f pidfile] [-l logfile] [-logger logger] [-p child_pidfile] [-P supervisor_pidfile] [-u user] command args ...\n\n", os.Args[0])
		fmt.Printf("  command\n        The command with arguments if any, to supervise.\n\n")
		flag.PrintDefaults()
	}
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
