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

	"gopkg.in/yaml.v3"
	"github.com/immortal/natcasesort"
)

// Parser interface
type Parser interface {
	Parse(fs *flag.FlagSet) (*Flags, error)
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
	fs.BoolVar(&p.Flags.Nodaemon, "n", false, "No daemon mode, stay in the foreground")
	fs.BoolVar(&p.Flags.Version, "v", false, "Print version")
	fs.IntVar(&p.Flags.Retries, "r", -1, "`number` of retries before program exit")
	fs.BoolVar(&p.Flags.CheckConfig, "cc", false, "Checks the config file")
	fs.StringVar(&p.Flags.ChildPid, "p", "", "Path to write the child `pidfile`")
	fs.StringVar(&p.Flags.Configfile, "c", "", "`run.yml` configuration file")
	fs.StringVar(&p.Flags.Ctl, "ctl", "", "Create supervise directory `/var/run/immortal/<service>`. Overrides `-name`")
	fs.StringVar(&p.Flags.Envdir, "e", "", "Set environment variables specified by files in the `dir`")
	fs.StringVar(&p.Flags.FollowPid, "f", "", "Follow PID in `pidfile`")
	fs.StringVar(&p.Flags.Logfile, "l", "", "Write stdout/stderr to `logfile`")
	fs.StringVar(&p.Flags.Logger, "logger", "", "A `command` to pipe stdout/stderr to stdin")
	fs.StringVar(&p.Flags.Name, "name", "", "A name for the service. This differs from -ctl in that it affects `${HOME}/.immortal/<name>`.")
	fs.StringVar(&p.Flags.ParentPid, "P", "", "Path to write the supervisor `pidfile`")
	fs.StringVar(&p.Flags.User, "u", "", "Execute command on behalf `user`")
	fs.StringVar(&p.Flags.Wrkdir, "d", "", "Change to `dir` before starting the command")
	fs.UintVar(&p.Flags.Wait, "w", 0, "`seconds` to wait before starting")

	err := fs.Parse(os.Args[1:])
	if err != nil {
		return nil, err
	}
	return &p.Flags, nil
}

func (p *Parse) parseYml(file string) (*Config, error) {
	f, err := ioutil.ReadFile(file)
	if err != nil {
		return nil, err
	}
	// set defaults
	var cfg = Config{Retries: -1}
	if err := yaml.Unmarshal(f, &cfg); err != nil {
		return nil, fmt.Errorf("unable to parse YAML file %q %s", file, err)
	}
	return &cfg, nil
}

func (p *Parse) checkWrkdir(dir string) error {
	if !isDir(dir) {
		return fmt.Errorf("-d %q does not exist or has wrong permissions, use (\"%s -h\") for help", dir, os.Args[0])
	}
	return nil
}

func (p *Parse) parseEnvdir(dir string) (map[string]string, error) {
	if !isDir(dir) {
		return nil, fmt.Errorf("-e %q does not exist or has wrong permissions, use (\"%s -h\") for help", dir, os.Args[0])
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

// checkUser needs cgo
func (p *Parse) checkUser(u string) (*user.User, error) {
	usr, err := p.UserLookup(u)
	if err != nil {
		if _, ok := err.(user.UnknownUserError); ok {
			return nil, fmt.Errorf("user %q does not exist", u)
		}
		return nil, fmt.Errorf("error looking up user: %q. %s", u, err)
	}
	return usr, nil
}

// Usage prints to standard error a usage message
func (p *Parse) Usage(fs *flag.FlagSet) func() {
	return func() {
		fmt.Fprintf(os.Stderr, "Usage: %s [-v] [-n] [-cc] [-c run.yml] [-ctl dir] [-d dir] [-e dir] [-f pidfile] [-l logfile] [-logger logger] [-p child_pidfile] [-P supervisor_pidfile] [-r retries] [-u user] [-w seconds] command\n\n   command\n        The command with arguments if any, to supervise\n\n", os.Args[0])
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

	// if -ctl, defaults to /var/run/immortal
	var sdir string
	if flags.Ctl != "" {
		if s := filepath.Clean(flags.Ctl); strings.HasPrefix(s, "/") {
			sdir = s
		} else {
			sdir = filepath.Join(GetSdir(), filepath.Base(s))
		}
	}

	// if -cc
	if flags.CheckConfig {
		if flags.Configfile == "" {
			err = fmt.Errorf("missing config file: -c run.yml, use (\"%s -h\") for help", os.Args[0])
		}
	}

	// if -c
	if flags.Configfile != "" {
		if !isFile(flags.Configfile) {
			err = fmt.Errorf("cannot read file: %q, use (\"%s -h\") for help", flags.Configfile, os.Args[0])
			return
		}

		// parse the `run.yml` file
		cfg, err = p.parseYml(flags.Configfile)
		if err != nil {
			return
		}

		// Save ConfigFile to use service name derived from file name instead of the PID
		cfg.configFile = flags.Configfile

		// Cmd is mandatory, is the command that needs to be supervised
		if cfg.Cmd == "" {
			err = fmt.Errorf("missing command, use (\"%s -h\") for help", os.Args[0])
			return
		}

		// split command into a slice of strings
		cfg.command = strings.Fields(cfg.Cmd)

		// Change working directory, will: cd Cwd before starting
		if cfg.Cwd != "" {
			if err = p.checkWrkdir(cfg.Cwd); err != nil {
				return
			}
		}

		// The user to run the process on behalf
		if cfg.User != "" {
			if cfg.user, err = p.checkUser(cfg.User); err != nil {
				return
			}
		}

		cfg.ctl = sdir

		// print config and exit 0 if config ok
		if flags.CheckConfig {
			y, _ := yaml.Marshal(&cfg)
			fmt.Printf("%s", y)
			return nil, nil
		}
		return
	}

	// if no args
	if len(fs.Args()) < 1 {
		err = fmt.Errorf("missing command, use (\"%s -h\") for help", os.Args[0])
		return
	}

	// create new cfg if not using run.yml
	cfg = new(Config)
	cfg.Log.Size = 1
	cfg.Retries = -1
	cfg.cli = true
	cfg.command = fs.Args()
	cfg.ctl = sdir
	if flags.Name != "" {
		cfg.name = flags.Name
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
		cfg.Pid.Follow = flags.FollowPid
	}

	// if -l
	if flags.Logfile != "" {
		cfg.Log.File = flags.Logfile
	}

	// if -logger
	if flags.Logger != "" {
		cfg.Logger = flags.Logger
	}

	// if -P
	if flags.ParentPid != "" {
		cfg.Pid.Parent = flags.ParentPid
	}

	// if -p
	if flags.ChildPid != "" {
		cfg.Pid.Child = flags.ChildPid
	}

	// if -r
	if flags.Retries >= 0 {
		cfg.Retries = flags.Retries
	}

	// if -w
	if flags.Wait > 0 {
		cfg.Wait = flags.Wait
	}

	// if -u
	if flags.User != "" {
		if cfg.user, err = p.checkUser(flags.User); err != nil {
			return
		}
	}
	return
}
