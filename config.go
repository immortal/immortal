package immortal

import (
	"os/user"
)

// Config yaml/command line configuration
type Config struct {
	Cmd     string            `yaml:"cmd" json:"cmd"`
	Cwd     string            `yaml:",omitempty" json:",omitempty"`
	Env     map[string]string `yaml:",omitempty" json:",omitempty"`
	Log     Log               `yaml:",omitempty" json:",omitempty"`
	StdErr  Log               `yaml:",omitempty" json:",omitempty"`
	StdOut  Log               `yaml:",omitempty" json:",omitempty"`
	Logger  string            `yaml:",omitempty" json:",omitempty"`
	Require []string          `yaml:",omitempty"`
	User    string            `yaml:",omitempty" json:",omitempty"`
	Wait    int               `yaml:",omitempty"`
	Pid     `yaml:",omitempty" json:",omitempty"`
	command []string
	ctl     string
	log     bool
	user    *user.User
}

// Pid struct run.yml
type Pid struct {
	Follow string `yaml:",omitempty"`
	Parent string `yaml:",omitempty"`
	Child  string `yaml:",omitempty"`
}

// Log struct run.yml
type Log struct {
	File      string `yaml:",omitempty"`
	Age       int    `yaml:",omitempty"`
	Num       int    `yaml:",omitempty"`
	Size      int    `yaml:",omitempty"`
	Timestamp bool   `yaml:",omitempty"`
}
