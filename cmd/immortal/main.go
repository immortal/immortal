package main

import (
	"flag"
	"fmt"
	"log"
	"log/syslog"
	"os"
	"os/user"

	"github.com/immortal/immortal"
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
	daemon, err := immortal.New(cfg)
	if err != nil {
		fmt.Fprintln(os.Stderr, err)
		log.Print(err)
		os.Exit(1)
	}

	// fork
	if os.Getppid() > 1 {
		if pid, err := daemon.Fork(); err != nil {
			log.Printf("Error while forking: %s", err)
			os.Exit(1)
		} else {
			if pid > 0 {
				fmt.Printf("%c  %d\n", immortal.Logo(), pid)
				os.Exit(0)
			}
		}
	}

	log.Printf("%c  %d", immortal.Logo(), os.Getpid())

	daemon.Run()
	immortal.Supervise(&immortal.Sup{}, daemon)
}
