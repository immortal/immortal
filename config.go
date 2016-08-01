package immortal

import (
	"os/user"
)

type Config struct {
	Cmd    string            `yaml:"cmd" json:"cmd"`
	Cwd    string            `yaml:",omitempty" json:",omitempty"`
	Env    map[string]string `yaml:",omitempty" json:",omitempty"`
	Pid    `yaml:",omitempty" json:",omitempty"`
	Log    `yaml:",omitempty" json:",omitempty"`
	Logger string `yaml:",omitempty" json:",omitempty"`
	User   string `yaml:",omitempty" json:",omitempty"`
	Wait   int    `yaml:",omitempty"`
	ctrl   bool
	user   *user.User
}

type Pid struct {
	Follow string `yaml:",omitempty"`
	Parent string `yaml:",omitempty"`
	Child  string `yaml:",omitempty"`
}

type Log struct {
	File string `yaml:",omitempty"`
	Age  int    `yaml:",omitempty"`
	Num  int    `yaml:",omitempty"`
	Size int    `yaml:",omitempty"`
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
