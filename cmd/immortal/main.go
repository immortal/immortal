package main

import (
	"fmt"
	"github.com/immortal/immortal"
	"os"
)

var version string

func main() {
	parser := &immortal.Parse{
		UserFinder: &immortal.User{},
	}

	flags, err := immortal.ParseFlags(parser)
	if err != nil {
		fmt.Fprintln(os.Stderr, err)
		os.Exit(1)
	}

	// if -v print version
	if flags.Version {
		fmt.Printf("%s\n", version)
		os.Exit(0)
	}

	fmt.Printf("%#v", flags)
}
