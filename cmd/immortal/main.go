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

func exists(path string) bool {
	if _, err := os.Stat(path); os.IsNotExist(err) {
		return false
	}
	return true
}

func is_exec(path string) (bool, error) {
	if f, err := os.Stat(path); err != nil {
		if os.IsNotExist(err) {
			return false, nil
		} else {
			return false, err
		}
	} else if m := f.Mode(); !m.IsDir() && m&0111 != 0 {
		return true, nil
	} else {
		return false, nil
	}
}

func main() {
	var (
		c      = flag.String("c", "", "`run.yml` configuration file")
		d      = flag.String("d", "", "Change to `dir` before starting the command")
		f      = flag.String("f", "", "Follow PID in `pidfile`")
		l      = flag.String("l", "", "Write stdout/stderr to `logfile`")
		logger = flag.String("logger", "", "A `command` to pipe stdout/stderr to stdin")
		p      = flag.String("p", "", "Path to write the child `pidfile`")
		P      = flag.String("P", "", "Path to write the supervisor `pidfile`")
		u      = flag.String("u", "", "Execute command on behalf `user`")
		ctrl   = flag.Bool("ctrl", false, "Create supervise directory")
		v      = flag.Bool("v", false, fmt.Sprintf("Print version: %s", version))
		err    error
		pid    int
		usr    *user.User
		D      *ir.Daemon
	)

	flag.Usage = func() {
		fmt.Fprintf(os.Stderr, "usage: %s [-v -ctrl] [-f pidfile] [-l logfile] [-logger logger] [-p child_pidfile] [-P supervisor_pidfile] [-u user] command\n\n", os.Args[0])
		fmt.Printf("  command\n        The command with arguments if any, to supervise.\n\n")
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
		fmt.Fprintf(os.Stderr, "Missing command, use (\"%s -h\") for help.\n", os.Args[0])
		os.Exit(1)
	}

	if *c != "" {
		if !exists(*c) {
			fmt.Printf("Cannot read file: %s, use -h for more info.\n", *c)
			os.Exit(1)
		}
	}

	if *d != "" {
		if !exists(*d) {
			fmt.Printf("-d %s does not exist or has wrong permissions.\n", *d)
			os.Exit(1)
		}
	}

	if *u != "" {
		usr, err = user.Lookup(*u)
		if err != nil {
			if _, ok := err.(user.UnknownUserError); ok {
				fmt.Printf("User %s does not exist.\n", *u)
			} else if err != nil {
				fmt.Printf("Error looking up user: %s\n", *u)
			}
			os.Exit(1)
		}
	}

	// log
	logwriter, err := syslog.New(syslog.LOG_NOTICE|syslog.LOG_DAEMON, "immortal")
	if err == nil {
		log.SetOutput(logwriter)
		log.SetFlags(0)
	} else {
		defer logwriter.Close()
	}

	// start Daemon
	D, err = ir.New(usr, c, d, f, l, logger, p, P, flag.Args(), ctrl)
	if err != nil {
		fmt.Println(err)
		os.Exit(1)
	}

	// fork
	if os.Getppid() > 1 {
		if pid, err = ir.Fork(); err != nil {
			log.Printf("Error while forking: %s", err)
			os.Exit(1)
		} else {
			if pid > 0 {
				fmt.Printf("%c  %d\n", ir.Logo(), pid)
				os.Exit(0)
			}
		}
	}

	log.Printf("%c  %d", ir.Logo(), os.Getpid())

	D.Logger()
	if *ctrl {
		D.Control()
	}
	D.Supervice()
}
