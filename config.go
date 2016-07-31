package immortal

import (
	"bufio"
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
	Wait   int    `yaml:",omitempty"`
	user   *user.User
}

type Log struct {
	File string `yaml:",omitempty"`
	Age  int    `yaml:",omitempty"`
	Num  int    `yaml:",omitempty"`
	Size int    `yaml:",omitempty"`
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

//func AA() {
//daemon := &Daemon{
//owner:   u,
//command: cmd,
//run: Run{
//Cwd:       *d,
//Env:       env,
//FollowPid: *f,
//Logfile:   *l,
//Logger:    *logger,
//ParentPid: *P,
//ChildPid:  *p,
//Ctrl:      *ctrl,
//},
//ctrl: Ctrl{
//fifo:  make(chan Return),
//quit:  make(chan struct{}),
//state: make(chan error),
//},
//}

//return daemon, nil
//}
