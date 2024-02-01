package immortal

// Flags available command flags
type Flags struct {
	CheckConfig bool
	ChildPid    string
	Command     string
	Configfile  string
	Ctl         string
	Envdir      string
	FollowPid   string
	Logfile     string
	Logger      string
	Name        string
	Nodaemon    bool
	ParentPid   string
	Retries     int
	User        string
	Version     bool
	Wait        uint
	Wrkdir      string
}
