package main

import (
	"flag"
	"fmt"
	"github.com/immortal/immortal"
	"log"
	"log/syslog"
	"os"
	"os/user"
)

var version string

func main() {
	parser := &immortal.Parse{
		UserLookup: user.Lookup,
	}

	// flag set
	fs := flag.NewFlagSet(os.Args[0], flag.ExitOnError)
	fs.Usage = parser.Usage(fs)

	cfg, err := immortal.ParseArgs(parser, fs)
	if err != nil {
		fmt.Fprintln(os.Stderr, err)
		os.Exit(1)
	}

	// if -v print version
	if (fs.Lookup("v")).Value.(flag.Getter).Get().(bool) {
		fmt.Printf("%s\n", version)
		os.Exit(0)
	}

	// log to syslog
	logger, err := syslog.New(syslog.LOG_NOTICE|syslog.LOG_DAEMON, "immortal")
	if err == nil {
		log.SetOutput(logger)
		log.SetFlags(0)
	} else {
		defer logger.Close()
	}

	// create daemon
	var daemon immortal.Immortal
	daemon, err = immortal.New(cfg)
	if err != nil {
		fmt.Fprintln(os.Stderr, err)
		os.Exit(1)
	}
	daemon.Fork()
	daemon.Run()
	daemon.Supervise()
}
