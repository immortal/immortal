package immortal

import (
	"flag"
	"fmt"
	"github.com/immortal/natcasesort"
	"gopkg.in/yaml.v2"
	"io/ioutil"
	"os"
	"os/user"
	"sort"
)

type Parser interface {
	Parse(fs *flag.FlagSet) (*Flags, error)
	isDir(path string) bool
	isFile(path string) bool
	parseYml(file string) (*Config, error)
	checkWrkdir(dir string) error
	checkEnvdir(dir string) error
	checkUser(user string) (*user.User, error)
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
	fs.StringVar(&self.Flags.ParentPid, "P", "", "Path to write the supervisor `pidfile`")
	fs.StringVar(&self.Flags.ChildPid, "p", "", "Path to write the child `pidfile`")
	fs.IntVar(&self.Flags.Seconds, "s", 0, "`seconds` to wait before starting")
	fs.StringVar(&self.Flags.User, "u", "", "Execute command on behalf `user`")

	err := fs.Parse(os.Args[1:])
	if err != nil {
		return nil, err
	}
	return &self.Flags, nil
}

func (self *Parse) isDir(path string) bool {
	f, err := os.Stat(path)
	if err != nil {
		return false
	}
	if m := f.Mode(); m.IsDir() && m&400 != 0 {
		return true
	}
	return false
}

func (self *Parse) isFile(path string) bool {
	f, err := os.Stat(path)
	if err != nil {
		return false
	}
	if m := f.Mode(); !m.IsDir() && m.IsRegular() && m&400 != 0 {
		return true
	}
	return false
}

func (self *Parse) parseYml(file string) (*Config, error) {
	f, err := ioutil.ReadFile(file)
	if err != nil {
		return nil, err
	}
	var cfg Config
	if err := yaml.Unmarshal(f, &cfg); err != nil {
		return nil, fmt.Errorf("Unable to parse YAML file %q %s", file, err)
	}
	return &cfg, nil
}

func (self *Parse) checkWrkdir(dir string) (err error) {
	if !self.isDir(dir) {
		err = fmt.Errorf("-d %q does not exist or has wrong permissions, use (\"%s -h\") for help.", dir, os.Args[0])
	}
	return
}

func (self *Parse) checkEnvdir(dir string) (err error) {
	if !self.isDir(dir) {
		err = fmt.Errorf("-e %q does not exist or has wrong permissions, use (\"%s -h\") for help.", dir, os.Args[0])
	}
	return
}

func (self *Parse) checkUser(u string) (usr *user.User, err error) {
	usr, err = user.Lookup(u)
	if err != nil {
		if _, ok := err.(user.UnknownUserError); ok {
			err = fmt.Errorf("User %q does not exist.", u)
		} else if err != nil {
			err = fmt.Errorf("Error looking up user: %q", u)
		}
		return
	}
	return
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

func ParseArgs(p Parser, fs *flag.FlagSet) (cfg *Config, err error) {
	flags, err := p.Parse(fs)
	if err != nil {
		return
	}

	// if -v
	if flags.Version {
		return
	}

	// if no args
	if len(fs.Args()) < 1 {
		err = fmt.Errorf("Missing command, use (\"%s -h\") for help.", os.Args[0])
		return
	}

	// if -c
	if flags.Configfile != "" {
		if !p.isFile(flags.Configfile) {
			err = fmt.Errorf("Cannot read file: %q, use (\"%s -h\") for help.", flags.Configfile, os.Args[0])
			return
		}
		cfg, err = p.parseYml(flags.Configfile)
		if err != nil {
			return
		}
	} else {
		cfg = new(Config)
	}

	// if -d
	if flags.Wrkdir != "" {
		if err = p.checkWrkdir(flags.Wrkdir); err != nil {
			return
		}
	}
	if cfg.Cwd != "" {
		if err = p.checkWrkdir(cfg.Cwd); err != nil {
			return
		}
	}

	// if -e
	if flags.Envdir != "" {
		if err = p.checkEnvdir(flags.Envdir); err != nil {
			return
		}
	}

	// if -u
	if flags.User != "" {
		if cfg.user, err = p.checkUser(flags.User); err != nil {
			return
		}
	}
	if cfg.User != "" {
		if cfg.user, err = p.checkUser(cfg.User); err != nil {
			return
		}
	}
	return
}
