package main

import (
	"flag"
	"fmt"
	ir "github.com/immortal/immortal"
	"os"
)

var version, githash string

func exists(x ir.Configuration, path string) bool {
	return x.Exists(path)
}

func is_exec(x ir.Configuration, path string) (bool, error) {
	return x.IsExec(path)
}

func main() {
	cfg := ir.NewConfig()
	cfg.Parse()

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
