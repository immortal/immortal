package main

import (
	"flag"
	"fmt"
	"log"
	"log/syslog"
	"os"
	"os/exec"
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

	// if -cc (check config)
	if (fs.Lookup("cc")).Value.(flag.Getter).Get().(bool) && cfg == nil {
		os.Exit(0)
	}

	// if -v print version
	if (fs.Lookup("v")).Value.(flag.Getter).Get().(bool) {
		fmt.Printf("%s\n", version)
		os.Exit(0)
	}

	// nodaemon check
	nodaemon := false
	if (fs.Lookup("n")).Value.(flag.Getter).Get().(bool) {
		nodaemon = true
	}

	// log to syslog
	logger, err := syslog.New(syslog.LOG_NOTICE|syslog.LOG_DAEMON, "immortal")
	if err == nil {
		log.SetOutput(logger)
		log.SetFlags(0)
	} else {
		defer logger.Close()
	}

	// check for required services to be UP otherwise don't start
	if len(cfg.Require) > 0 {
		down := []string{}
		ctl := &immortal.Controller{}
		userSdir, err := immortal.GetUserSdir()
		if err != nil {
			log.Fatal(err)
		}
		for _, r := range cfg.Require {
			sockets := []string{
				filepath.Join(immortal.GetSdir(), r, "immortal.sock"),
				filepath.Join(userSdir, r, "immortal.sock"),
			}
			isDown := true
			for _, socket := range sockets {
				status, err := ctl.GetStatus(socket)
				if err == nil && status.Up != "" {
					isDown = false
					break
				}
			}
			if isDown {
				down = append(down, r)
			}
		}
		if len(down) > 0 {
			log.Fatalf("required services are not UP: %s", strings.Join(down, ", "))
		}
	}

	// Check for required command exit to be 0
	if len(cfg.RequireCmd) > 0 {
		var shell = "sh"
		if sh := os.Getenv("SHELL"); sh != "" {
			shell = sh
		}
		if err := exec.Command(shell, "-c", cfg.RequireCmd).Run(); err != nil {
			log.Fatalf("required command failed: %s", err)
		}
	}

	// fork
	if !nodaemon && os.Getppid() > 1 {
		if pid, err := immortal.Fork(); err != nil {
			log.Fatalf("error while forking: %s", err)
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

	// print logo and process pid
	log.Printf("%c  %d", 0x2B55, os.Getpid())

	// listen on socket
	if err := daemon.Listen(); err != nil {
		log.Fatal(err)
	}

	// Supervise the process
	if err := immortal.Supervise(daemon); err != nil {
		log.Fatal(err)
	}
}
