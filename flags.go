package immortal

// Flags available command flags
type Flags struct {
	Version    bool
	Seconds    int
	Configfile string
	Wrkdir     string
	Envdir     string
	FollowPid  string
	Logfile    string
	Logger     string
	ParentPid  string
	ChildPid   string
	User       string
	Command    string
	Ctl        string
}
