package immortal

import (
	"flag"
	"fmt"
	"github.com/immortal/natcasesort"
	"os"
	"os/user"
	"sort"
)

type Parser interface {
	Parse(fs *flag.FlagSet) (*Flags, error)
	exists(path string) bool
}

type Parse struct {
	Flags
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
	f, err := os.Stat(path)
	if err != nil {
		return false
	}
	if m := f.Mode(); m.IsDir() && m&400 != 0 {
		return true
	}
	return false
}

func (self *Parse) Usage(fs *flag.FlagSet) func() {
	return func() {
		fmt.Fprintf(os.Stderr, "Usage: %s [-v -ctrl] [-d dir] [-e dir] [-f pidfile] [-l logfile] [-logger logger] [-p child_pidfile] [-P supervisor_pidfile] [-u user] command\n\n   command\n        The command with arguments if any, to supervise\n\n", os.Args[0])
		var flags []string
		fs.VisitAll(func(f *flag.Flag) {
			flags = append(flags, f.Name)
		})
		sort.Sort(natcasesort.Sort(flags))
		for _, v := range flags {
			f := fs.Lookup(v)
			s := fmt.Sprintf("  -%s", f.Name)
			name, usage := flag.UnquoteUsage(f)
			if len(name) > 0 {
				s += " " + name
			}
			if len(s) <= 4 {
				s += "\t"
			} else {
				s += "\n    \t"
			}
			s += usage
			fmt.Fprintf(os.Stderr, "%s\n", s)
		}
	}
}

func ParseArgs(p Parser, fs *flag.FlagSet) (*Flags, error) {
	flags, err := p.Parse(fs)
	if err != nil {
		return nil, err
	}

	// if -v
	if flags.Version {
		return flags, nil
	}

	// if no args
	if len(fs.Args()) < 1 {
		return nil, fmt.Errorf("Missing command, use (\"%s -h\") for help.", os.Args[0])
	}

	// if -c
	if flags.Configfile != "" {
		if !p.exists(flags.Configfile) {
			return nil, fmt.Errorf("Cannot read file: %q, use (\"%s -h\") for help.", flags.Configfile, os.Args[0])
		}
	}

	// if -d
	if flags.Wrkdir != "" {
		if !p.exists(flags.Wrkdir) {
			return nil, fmt.Errorf("-d %q does not exist or has wrong permissions, use (\"%s -h\") for help.", flags.Wrkdir, os.Args[0])
		}
	}

	// if -e
	if flags.Envdir != "" {
		if !p.exists(flags.Envdir) {
			return nil, fmt.Errorf("-e %q does not exist or has wrong permissions, use (\"%s -h\") for help.", flags.Envdir, os.Args[0])
		}
	}

	// if -u
	if flags.User != "" {
		usr, err := user.Lookup(flags.User)
		if err != nil {
			if _, ok := err.(user.UnknownUserError); ok {
				return nil, fmt.Errorf("User %q does not exist.", flags.User)
			} else if err != nil {
				return nil, fmt.Errorf("Error looking up user: %q", flags.User)
			}
		}
		flags.user = usr
	}

	return flags, nil
}
