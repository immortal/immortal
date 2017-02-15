package main

import (
	"flag"
	"fmt"
	"os"
)

var version string

// immortal-ctl options service
// immortal-ctl status (print status of all services)
func main() {
	var (
		v    = flag.Bool("v", false, fmt.Sprintf("Print version: %s", version))
		sdir string
	)

	// if IMMORTAL_SDIR env is set, use it as default sdir
	// TODO  how to handle errors when dir don't exists
	if sdir = os.Getenv("IMMORTAL_SDIR"); sdir == "" {
		sdir = "/var/run/immortal"
	}

	flag.Usage = func() {
		fmt.Fprintf(os.Stderr, "usage: %s [option] [signal] service\n\n%s\n%s\n%s\n%s\n%s\n%s\n\n%s\n%s\n%s\n%s\n%s\n%s\n%s\n%s\n%s\n%s\n%s\n%s\n%s\n%s\n%s\n\n",
			os.Args[0],
			"  Options:",
			"    start     Start the service",
			"    stop      Stop the service",
			"    restart   Restart the service",
			"    once      Do not restart if service stops",
			"    kill      Terminate service",
			"  Signals:",
			"    a, alrm   ALRM",
			"    c, cont   CONT",
			"    d, down   TERM",
			"    h, hup    HUP",
			"    i, int    INT",
			"    in, TTIN  TTIN",
			"    ou, TTOU  TTOU",
			"    s, stop   STOP",
			"    q, quit   QUIT",
			"    t, term   TERM",
			"    q, quit   QUIT",
			"    1, usr1   USR1",
			"    2, usr2   USR2",
			"    w, winch  WINCH",
		)
		flag.PrintDefaults()
	}

	flag.Parse()

	if *v {
		fmt.Printf("%s\n", version)
		os.Exit(0)
	}
}
