package immortal

import (
	"os/user"
)

type Flags struct {
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
	user       *user.User
	Command    string
}
