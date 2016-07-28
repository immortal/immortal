package main

import (
	"flag"
	"fmt"
	ir "github.com/immortal/immortal"
	"os"
)

var version string

func exists(x ir.Config, path string) bool {
	return x.Exists(path)
}

func main() {
	cfg := ir.New()
	cfg.Parser.Parse(&cfg.Flags)

	// print version
	if cfg.Version {
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
}
