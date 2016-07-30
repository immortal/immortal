package immortal

import (
	"bufio"
	"gopkg.in/yaml.v2"
	"io/ioutil"
	"os"
	"os/user"
	"path/filepath"
)

type Config struct {
	Cmd    string            `yaml:"cmd" json:"cmd"`
	Cwd    string            `yaml:",omitempty" json:",omitempty"`
	Env    map[string]string `yaml:",omitempty" json:",omitempty"`
	Pid    map[string]string `yaml:",omitempty" json:",omitempty"`
	Log    `yaml:",omitempty" json:",omitempty"`
	Logger string `yaml:",omitempty" json:",omitempty"`
	User   string `yaml:",omitempty" json:",omitempty"`
}

type Log struct {
	File string `yaml:",omitempty"`
	Age  int    `yaml:",omitempty"`
	Num  int    `yaml:",omitempty"`
	Size int    `yaml:",omitempty"`
}

func ParseYml(file string) (*Config, error) {
	f, err := ioutil.ReadFile(file)
	if err != nil {
		return nil, err
	}
	var cfg Config
	if err := yaml.Unmarshal(f, &cfg); err != nil {
		return nil, err
	}
	return &cfg, nil
}

func (self *Config) GetEnv(dir string) (map[string]string, error) {
	files, err := ioutil.ReadDir(dir)
	if err != nil {
		return nil, err
	}
	env := make(map[string]string)
	for _, f := range files {
		if f.Mode().IsRegular() {
			lines := 0
			ff, err := os.Open(filepath.Join(dir, f.Name()))
			if err != nil {
				continue
			}
			defer ff.Close()
			s := bufio.NewScanner(ff)
			for s.Scan() {
				if lines >= 1 {
					break
				}
				env[f.Name()] = s.Text()
				lines++
			}
		}
	}
	return env, nil
}

func (self *Config) Exists(path string) bool {
	if _, err := os.Stat(path); os.IsNotExist(err) {
		return false
	}
	return true
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
func NewOld(u *user.User, c, d, e, f, l, logger, p, P *string, cmd []string, ctrl *bool) (*Daemon, error) {
	var (
		env map[string]string
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
	//if *e != "" {
	//env, err = GetEnv(*e)
	//if err != nil {
	//return nil, err
	//}
	//}

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

	return daemon, nil
}
