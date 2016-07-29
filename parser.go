package immortal

import (
	"flag"
	"fmt"
	"os"
)

type Parser interface {
	Parse() *Flags
	exists(path string) bool
	UserFinder
}

type Parse struct {
	Flags
	UserFinder
	// for testing only
	args []string
}

func (self *Parse) Parse() *Flags {
	flag.BoolVar(&self.Flags.Ctrl, "ctrl", false, "Create supervise directory")
	flag.BoolVar(&self.Flags.Version, "v", false, "Print version")
	flag.StringVar(&self.Flags.Configfile, "c", "", "`run.yml` configuration file")
	flag.StringVar(&self.Flags.Wrkdir, "d", "", "Change to `dir` before starting the command")
	flag.StringVar(&self.Flags.Envdir, "e", "", "Set environment variables specified by files in the `dir`")
	flag.StringVar(&self.Flags.FollowPid, "f", "", "Follow PID in `pidfile`")
	flag.StringVar(&self.Flags.Logfile, "l", "", "Write stdout/stderr to `logfile`")
	flag.StringVar(&self.Flags.Logger, "logger", "", "A `command` to pipe stdout/stderr to stdin")
	flag.StringVar(&self.Flags.ChildPid, "p", "", "Path to write the child `pidfile`")
	flag.StringVar(&self.Flags.ParentPid, "P", "", "Path to write the supervisor `pidfile`")
	flag.StringVar(&self.Flags.User, "u", "", "Execute command on behalf `user`")

	flag.Usage = func() {
		fmt.Fprintf(os.Stderr, "usage: %s [-v -ctrl] [-d dir] [-e dir] [-f pidfile] [-l logfile] [-logger logger] [-p child_pidfile] [-P supervisor_pidfile] [-u user] command args\n\n", os.Args[0])
		fmt.Printf("  command\n        The command with arguments if any, to supervise.\n\n")
		flag.PrintDefaults()
	}

	a := os.Args[1:]
	if self.args != nil {
		a = self.args
	}

	flag.CommandLine.Parse(a)

	return &self.Flags
}

func (self *Parse) exists(path string) bool {
	if _, err := os.Stat(path); err != nil {
		return false
	}
	return true
}

func ParseFlags(p Parser) (*Flags, error) {
	flags := p.Parse()

	// if -v
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
