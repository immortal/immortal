package main

import (
	"flag"
	"fmt"
	"github.com/immortal/immortal"
	"os"
)

var version string

func main() {
	parser := new(immortal.Parse)

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

	// create a new daemon
	//	daemon = new(immortal.Daemon)

	fmt.Printf("%v", cfg)
}
