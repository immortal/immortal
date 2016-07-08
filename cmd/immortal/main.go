package main

import (
	"flag"
	"fmt"
	ir "github.com/immortal/immortal"
	"os"
	"os/user"
	"time"
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

	D, err = ir.New(usr, c, p, l)
	if err != nil {
		fmt.Println(err)
		os.Exit(1)
	}

	D.Fork()

	D.Run(flag.Args())

	for {

		select {
		case pid := <-D.Pid:
			ir.Log(ir.Green(fmt.Sprintf("pid %v:", pid)))
		case err := <-D.Status:
			if err != nil {
				ir.Log(ir.Red(err.Error()))
			}
			time.Sleep(1 * time.Second)
		default:
			D.Run(flag.Args())
		}
	}
}

// run forever until ctrl+c or kill signal
//	D.Block()
