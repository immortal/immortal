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
		u     = flag.Bool("u", false, "If the service is not running, `start` it.")
		d     = flag.Bool("d", false, "If the service is running, `stop` it.")
		a     = flag.Bool("a", false, "alarm")
		c     = flag.Bool("c", false, "cont")
		h     = flag.Bool("hup", false, "hup")
		i     = flag.Bool("i", false, "int")
		in    = flag.Bool("in", false, "TTIN")
		k     = flag.Bool("k", false, "kill")
		o     = flag.Bool("o", false, "once")
		out   = flag.Bool("out", false, "out")
		s     = flag.Bool("s", false, "stop")
		q     = flag.Bool("q", false, "quit")
		t     = flag.Bool("t", false, "term")
		usr1  = flag.Bool("1", false, "usr1")
		usr2  = flag.Bool("2", false, "usr2")
		winch = flag.Bool("w", false, "winch")

		v    = flag.Bool("v", false, fmt.Sprintf("Print version: %s", version))
		sdir string
	)

	// if IMMORTAL_SDIR env is set, use it as default sdir
	// TODO  how to handle errors when dir don't exists
	if sdir = os.Getenv("IMMORTAL_SDIR"); sdir == "" {
		sdir = "/var/run/immortal"
	}

	flag.Usage = func() {
		fmt.Fprintf(os.Stderr, "usage: %s [options] [signal] [sdir] service\n\n%s\n%s\n%s\n%s\n\n",
			os.Args[0],
			"  Options:",
			"    start     Start the service",
			"    stop      Stop the service",
			"    restart   Restart the service")
		flag.PrintDefaults()
	}

	flag.Parse()

	if *v {
		fmt.Printf("%s\n", version)
		os.Exit(0)
	}
	fmt.Printf("*u = %+v\n", *u)
	fmt.Printf("*d = %+v\n", *d)
	println(a, c, h, i, in, k, o, out, s, q, t, usr1, usr2, winch)

}
