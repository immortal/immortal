package main

import (
	"flag"
	"fmt"
	ir "github.com/immortal/immortal"
	"log"
	"os"
	"os/user"
)

var version, githash string

func main() {
	var (
		p   = flag.String("p", "", "PID file")
		l   = flag.String("l", "", "Log path")
		u   = flag.String("u", "", "Execute command on behalf user")
		v   = flag.Bool("v", false, fmt.Sprintf("Print version: %s", version))
		c   = flag.String("c", "", "run.yml configuration file")
		err error
		usr *user.User
		D   *ir.Daemon
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
		fmt.Fprintf(os.Stderr, "Missing command (\"%s -h\") for help", os.Args[0])
		os.Exit(1)
	}

	if *c != "" {
		if _, err = os.Stat(*c); os.IsNotExist(err) {
			fmt.Printf("Cannot read file: %s, use -h for more info.\n\n", *c)
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
	f, err := os.OpenFile("/tmp/immortal.log", os.O_CREATE|os.O_APPEND|os.O_WRONLY, 0600)
	if err != nil {
		fmt.Println(err.Error())
		os.Exit(1)
	}
	defer f.Close()
	log.SetOutput(f)
	log.SetFlags(0)

	D, err = ir.New(usr, c, p, l, flag.Args())
	if err != nil {
		fmt.Println(err)
		os.Exit(1)
	}

	// only one instance
	if err := D.Lock(); err != nil {
		fmt.Println("Another instance of immortal is running")
		os.Exit(1)
	}

	D.Fork()
	err = D.FIFO()
	if err != nil {
		ir.Log(err.Error())
		os.Exit(1)
	}
	D.Supervice()
}
