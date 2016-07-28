package immortal

import (
	"flag"
	"fmt"
	"os"
)

type ParserI interface {
	Parse(flags *Flags)
}

type Parser struct{}

func (self *Parser) Parse(flags *Flags) {
	flag.BoolVar(&flags.Ctrl, "ctrl", false, "Create supervise directory")
	flag.BoolVar(&flags.Version, "v", false, "Print version")
	flag.StringVar(&flags.Configfile, "c", "", "`run.yml` configuration file")
	flag.StringVar(&flags.Wrkdir, "d", "", "Change to `dir` before starting the command")
	flag.StringVar(&flags.Envdir, "e", "", "Set environment variables specified by files in the `dir`")
	flag.StringVar(&flags.FollowPid, "f", "", "Follow PID in `pidfile`")
	flag.StringVar(&flags.Logfile, "l", "", "Write stdout/stderr to `logfile`")
	flag.StringVar(&flags.Logger, "logger", "", "A `command` to pipe stdout/stderr to stdin")
	flag.StringVar(&flags.ChildPid, "p", "", "Path to write the child `pidfile`")
	flag.StringVar(&flags.ParentPid, "P", "", "Path to write the supervisor `pidfile`")
	flag.StringVar(&flags.User, "u", "", "Execute command on behalf `user`")

	flag.Usage = func() {
		fmt.Fprintf(os.Stderr, "usage: %s [-v -ctrl] [-d dir] [-e dir] [-f pidfile] [-l logfile] [-logger logger] [-p child_pidfile] [-P supervisor_pidfile] [-u user] command args ...\n\n", os.Args[0])
		fmt.Printf("  command\n        The command with arguments if any, to supervise.\n\n")
		flag.PrintDefaults()
	}

	flag.CommandLine.Parse(os.Args[1:])
}
