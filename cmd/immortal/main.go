package main

import (
	"flag"
	"fmt"
	"log"
	"log/syslog"
	"os"
	"os/user"
	"path/filepath"
	"strings"

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

	// check for dependencies to be up and running otherwise don't start
	if len(cfg.Require) > 0 {
		halt := false
		ctl := &immortal.Controller{}
		for _, r := range cfg.Require {
			socket := filepath.Join(immortal.GetSdir(), r, "immortal.sock")
			if status, err := ctl.GetStatus(socket); err != nil {
				halt = true
			} else if status.Up == "" {
				halt = true
			}
		}
		if halt {
			log.Fatalf("required services are not UP: %s", strings.Join(cfg.Require))
		}
	}

	// fork
	if os.Getppid() > 1 {
		if pid, err := immortal.Fork(); err != nil {
			log.Fatalf("Error while forking: %s", err)
		} else {
			if pid > 0 {
				os.Exit(0)
			}
		}
	}

	// create daemon
	daemon, err := immortal.New(cfg)
	if err != nil {
		log.Fatal(err)
	}

	log.Printf("%c  %d", immortal.Logo(), os.Getpid())

	// listen on socket
	if err := daemon.Listen(); err != nil {
		log.Fatal(err)
	}

	immortal.Supervise(daemon)
}
