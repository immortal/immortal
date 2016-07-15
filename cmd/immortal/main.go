package main

import (
	"flag"
	"fmt"
	ir "github.com/immortal/immortal"
	"log"
	"log/syslog"
	"os"
	"os/user"
)

var version, githash string

func main() {
	var (
		c      = flag.String("c", "", "run.yml configuration file")
		l      = flag.String("l", "", "Log file path")
		logger = flag.String("logger", "", "External logger to use")
		p      = flag.String("p", "", "PID file")
		u      = flag.String("u", "", "Execute command on behalf user")
		ctrl   = flag.Bool("ctrl", false, fmt.Sprintf("Print version: %s", version))
		v      = flag.Bool("v", false, fmt.Sprintf("Print version: %s", version))
		err    error
		usr    *user.User
		D      *ir.Daemon
		pid    int
	)

	flag.Usage = func() {
		fmt.Fprintf(os.Stderr, "usage: %s [-qv] [-p /pid/file] [-l /log/path/] [-u user] command arguments\n\n", os.Args[0])
		fmt.Printf("  command   The command to supervise.\n\n")
		flag.PrintDefaults()
	}

	flag.Parse()

	// if v print version
	if *v {
		if githash != "" {
			fmt.Printf("%s+%s\n", version, githash)
		} else {
			fmt.Printf("%s\n", version)
		}
		os.Exit(0)
	}

	// if no args exit
	if len(flag.Args()) < 1 {
		fmt.Fprintf(os.Stderr, "Missing command, use (\"%s -h\") for help", os.Args[0])
		os.Exit(1)
	}

	if *c != "" {
		if !exists(*c) {
			fmt.Printf("Cannot read file: %s, use -h for more info.\n\n", *c)
			os.Exit(1)
		}
	}

	if *logger != "" {
		if _, err = is_exec(*logger); err != nil {
			fmt.Printf("logger error: %s, use -h for more info.\n\n", err)
			os.Exit(1)
		}
	}

	if *u != "" {
		usr, err = user.Lookup(*u)
		if err != nil {
			if _, ok := err.(user.UnknownUserError); ok {
				fmt.Printf("User %s does not exist.", *u)
			} else if err != nil {
				fmt.Printf("Error looking up user: %s", *u)
			}
			os.Exit(1)
		}
	}

	// log
	logwriter, err := syslog.New(syslog.LOG_NOTICE|syslog.LOG_DAEMON, "immortal")
	if err == nil {
		log.SetOutput(logwriter)
	}

	D, err = ir.New(usr, c, p, l, logger, flag.Args(), ctrl)
	if err != nil {
		fmt.Println(err)
		os.Exit(1)
	}

	// only one instance
	if err = D.Lock(); err != nil {
		log.Println("Another instance of immortal is running")
		os.Exit(1)
	}

	if pid, err = D.Fork(); err != nil {
		fmt.Println("check path: ", err.Error())
		os.Exit(1)
	} else {
		if pid > 0 {
			fmt.Printf("%c  %d", ir.Icon("2B55"), pid)
			log.Printf("%c  %d", ir.Icon("2B55"), pid)
			os.Exit(0)
		}
	}

	D.Supervice()
}
