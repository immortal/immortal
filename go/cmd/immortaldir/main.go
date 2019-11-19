package main

import (
	"flag"
	"fmt"
	"os"

	"github.com/immortal/immortal"
)

var version string

func main() {
	var (
		v    = flag.Bool("v", false, fmt.Sprintf("Print version: %s", version))
		path string
	)

	flag.Usage = func() {
		fmt.Fprintf(os.Stderr, "usage: %s [dir]\n\n", os.Args[0])
		fmt.Printf("  dir   The directory that will be scanned.\n")
		flag.PrintDefaults()
	}

	flag.Parse()

	if *v {
		fmt.Printf("%s\n", version)
		os.Exit(0)
	}

	if len(flag.Args()) >= 1 {
		path = flag.Args()[0]
	} else {
		flag.Usage()
		os.Exit(1)
	}

	cmd, err := immortal.NewScanDir(path)
	if err != nil {
		fmt.Fprintln(os.Stderr, err)
		os.Exit(1)
	}

	ctl := &immortal.Controller{}
	cmd.Start(ctl)
}
