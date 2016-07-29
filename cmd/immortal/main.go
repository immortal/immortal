package main

import (
	"flag"
	"fmt"
	"github.com/immortal/immortal"
	"os"
)

var version string

func main() {
	parser := &immortal.Parse{
		UserFinder: &immortal.User{},
	}

	// flag set
	fs := flag.NewFlagSet(os.Args[0], flag.ExitOnError)
	fs.Usage = func() {
		fmt.Fprintf(os.Stderr, "Usage: %s [-v -ctrl] [-d dir] [-e dir] [-f pidfile] [-l logfile] [-logger logger] [-p child_pidfile] [-P supervisor_pidfile] [-u user] command\n\n   command\n        The command with arguments if any, to supervise\n\n", os.Args[0])
		fs.PrintDefaults()
	}

	flags, err := immortal.ParseArgs(parser, fs)
	if err != nil {
		fmt.Fprintln(os.Stderr, err)
		os.Exit(1)
	}

	//	 if -v print version
	if flags.Version {
		fmt.Printf("%s\n", version)
		os.Exit(0)
	}

	fmt.Printf("%#v", flags)
}
