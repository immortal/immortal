package immortal

import (
	"bufio"
	"flag"
	"fmt"
	"io/ioutil"
	"os"
	"os/user"
	"path/filepath"
	"sort"
	"strings"

	"github.com/immortal/natcasesort"
	"gopkg.in/yaml.v2"
)

// Parser interface
type Parser interface {
	Parse(fs *flag.FlagSet) (*Flags, error)
	isDir(path string) bool
	isFile(path string) bool
	parseYml(file string) (*Config, error)
	checkWrkdir(dir string) error
	parseEnvdir(dir string) (map[string]string, error)
	checkUser(username string) (*user.User, error)
}

// Parse implements parser
type Parse struct {
	Flags
	UserLookup func(username string) (*user.User, error)
}

// Parse parse the command line flags
func (p *Parse) Parse(fs *flag.FlagSet) (*Flags, error) {
	fs.BoolVar(&p.Flags.Ctrl, "ctrl", false, "Create supervise directory")
	fs.BoolVar(&p.Flags.Version, "v", false, "Print version")
	fs.StringVar(&p.Flags.Configfile, "c", "", "`run.yml` configuration file")
	fs.StringVar(&p.Flags.Wrkdir, "d", "", "Change to `dir` before starting the command")
	fs.StringVar(&p.Flags.Envdir, "e", "", "Set environment variables specified by files in the `dir`")
	fs.StringVar(&p.Flags.FollowPid, "f", "", "Follow PID in `pidfile`")
	fs.StringVar(&p.Flags.Logfile, "l", "", "Write stdout/stderr to `logfile`")
	fs.StringVar(&p.Flags.Logger, "logger", "", "A `command` to pipe stdout/stderr to stdin")
	fs.StringVar(&p.Flags.ParentPid, "P", "", "Path to write the supervisor `pidfile`")
	fs.StringVar(&p.Flags.ChildPid, "p", "", "Path to write the child `pidfile`")
	fs.IntVar(&p.Flags.Seconds, "s", 0, "`seconds` to wait before starting")
	fs.StringVar(&p.Flags.User, "u", "", "Execute command on behalf `user`")

	err := fs.Parse(os.Args[1:])
	if err != nil {
		return nil, err
	}
	return &p.Flags, nil
}

func (p *Parse) isDir(path string) bool {
	f, err := os.Stat(path)
	if err != nil {
		return false
	}
	if m := f.Mode(); m.IsDir() && m&400 != 0 {
		return true
	}
	return false
}

func (p *Parse) isFile(path string) bool {
	f, err := os.Stat(path)
	if err != nil {
		return false
	}
	if m := f.Mode(); !m.IsDir() && m.IsRegular() && m&400 != 0 {
		return true
	}
	return false
}

func (p *Parse) parseYml(file string) (*Config, error) {
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

func (p *Parse) checkWrkdir(dir string) (err error) {
	if !p.isDir(dir) {
		err = fmt.Errorf("-d %q does not exist or has wrong permissions, use (\"%s -h\") for help.", dir, os.Args[0])
	}
	return
}

func (p *Parse) parseEnvdir(dir string) (map[string]string, error) {
	if !p.isDir(dir) {
		return nil, fmt.Errorf("-e %q does not exist or has wrong permissions, use (\"%s -h\") for help.", dir, os.Args[0])
	}
	files, err := ioutil.ReadDir(dir)
	if err != nil {
		return nil, err
	}
	env := make(map[string]string)
	for _, f := range files {
		if f.Mode().IsRegular() {
			lines := 0
			ff, err := os.Open(filepath.Join(dir, f.Name()))
			if err != nil {
				continue
			}
			defer ff.Close()
			s := bufio.NewScanner(ff)
			for s.Scan() {
				if lines >= 1 {
					break
				}
				env[f.Name()] = s.Text()
				lines++
			}
		}
	}
	return env, nil
}

func (p *Parse) checkUser(u string) (usr *user.User, err error) {
	usr, err = p.UserLookup(u)
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

// Usage prints to standard error a usage message
func (p *Parse) Usage(fs *flag.FlagSet) func() {
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

// ParseArgs parse command arguments
func ParseArgs(p Parser, fs *flag.FlagSet) (cfg *Config, err error) {
	flags, err := p.Parse(fs)
	if err != nil {
		return
	}

	// if -v
	if flags.Version {
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
		if cfg.Cmd == "" {
			err = fmt.Errorf("Missing command, use (\"%s -h\") for help.", os.Args[0])
			return
		} else {
			cfg.command = strings.Fields(cfg.Cmd)
		}
		if cfg.Cwd != "" {
			if err = p.checkWrkdir(cfg.Cwd); err != nil {
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

	// if no args
	if len(fs.Args()) < 1 {
		err = fmt.Errorf("Missing command, use (\"%s -h\") for help.", os.Args[0])
		return
	}

	// create new cfg if not using run.yml
	cfg = new(Config)
	cfg.command = fs.Args()

	// if -ctrl
	if flags.Ctrl {
		cfg.ctrl = true
	}

	// if -d
	if flags.Wrkdir != "" {
		if err = p.checkWrkdir(flags.Wrkdir); err != nil {
			return
		}
		cfg.Cwd = flags.Wrkdir
	}

	// if -e
	if flags.Envdir != "" {
		if cfg.Env, err = p.parseEnvdir(flags.Envdir); err != nil {
			return
		}
	}

	// if -f
	if flags.FollowPid != "" {
		cfg.Follow = flags.FollowPid
	}

	// if -l
	if flags.Logfile != "" {
		cfg.File = flags.Logfile
	}

	// if -logger
	if flags.Logger != "" {
		cfg.Logger = flags.Logger
	}

	// if -P
	if flags.ParentPid != "" {
		cfg.Parent = flags.ParentPid
	}

	// if -p
	if flags.ChildPid != "" {
		cfg.Child = flags.ChildPid
	}

	// if -s
	if flags.Seconds > 0 {
		cfg.Wait = flags.Seconds
	}

	// if -u
	if flags.User != "" {
		if cfg.user, err = p.checkUser(flags.User); err != nil {
			return
		}
	}
	return
}
