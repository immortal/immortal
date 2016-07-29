package immortal

import (
	"flag"
	"fmt"
	"os"
)

type Parser interface {
	Parse(fs *flag.FlagSet) (*Flags, error)
	exists(path string) bool
	UserFinder
}

type Parse struct {
	Flags
	UserFinder
}

func (self *Parse) Parse(fs *flag.FlagSet) (*Flags, error) {
	fs.BoolVar(&self.Flags.Ctrl, "ctrl", false, "Create supervise directory")
	fs.BoolVar(&self.Flags.Version, "v", false, "Print version")
	fs.StringVar(&self.Flags.Configfile, "c", "", "`run.yml` configuration file")
	fs.StringVar(&self.Flags.Wrkdir, "d", "", "Change to `dir` before starting the command")
	fs.StringVar(&self.Flags.Envdir, "e", "", "Set environment variables specified by files in the `dir`")
	fs.StringVar(&self.Flags.FollowPid, "f", "", "Follow PID in `pidfile`")
	fs.StringVar(&self.Flags.Logfile, "l", "", "Write stdout/stderr to `logfile`")
	fs.StringVar(&self.Flags.Logger, "logger", "", "A `command` to pipe stdout/stderr to stdin")
	fs.StringVar(&self.Flags.ChildPid, "p", "", "Path to write the child `pidfile`")
	fs.StringVar(&self.Flags.ParentPid, "P", "", "Path to write the supervisor `pidfile`")
	fs.StringVar(&self.Flags.User, "u", "", "Execute command on behalf `user`")

	err := fs.Parse(os.Args[1:])
	if err != nil {
		return nil, err
	}
	return &self.Flags, nil
}

func (self *Parse) exists(path string) bool {
	if _, err := os.Stat(path); err != nil {
		return false
	}
	return true
}

func ParseArgs(p Parser, fs *flag.FlagSet) (*Flags, error) {
	flags, err := p.Parse(fs)
	if err != nil {
		return nil, err
	}

	//	 if -v
	if flags.Version {
		return flags, nil
	}

	// if no args
	if len(flag.Args()) < 1 {
		return nil, fmt.Errorf("Missing command, use (\"%s -h\") for help.", os.Args[0])
	}

	// if -c
	if flags.Configfile != "" {
		if !p.exists(flags.Configfile) {
			return nil, fmt.Errorf("Cannot read file: %q, use (\"%s -h\") for help.", flags.Configfile, os.Args[0])
			os.Exit(1)
		}
	}

	// if -d
	if flags.Wrkdir != "" {
		if !p.exists(flags.Wrkdir) {
			return nil, fmt.Errorf("-d %q does not exist or has wrong permissions, use (\"%s -h\") for help.", flags.Wrkdir, os.Args[0])
		}
	}

	// if -u
	if flags.User != "" {
		usr, err := p.Lookup(flags.User)
		if err != nil {
			return nil, err
		}
		flags.user = usr
	}

	return flags, nil
}
