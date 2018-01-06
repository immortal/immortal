package immortal

// Flags available command flags
type Flags struct {
	ChildPid   string
	Command    string
	Configfile string
	Ctl        string
	Envdir     string
	FollowPid  string
	Logfile    string
	Logger     string
	ParentPid  string
	Retries    uint
	User       string
	Version    bool
	Wait       uint
	Wrkdir     string
}
