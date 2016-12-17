package immortal

// Flags available command flags
type Flags struct {
	Ctl        bool
	Version    bool
	Configfile string
	Wrkdir     string
	Envdir     string
	FollowPid  string
	Logfile    string
	Logger     string
	ParentPid  string
	ChildPid   string
	User       string
	Seconds    int
	Command    string
}
