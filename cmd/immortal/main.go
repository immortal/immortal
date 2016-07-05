package main

import (
	"flag"
	"fmt"
	ir "github.com/immortal/go"
	"os"
)

var version, githash string

func main() {
	var f = flag.String("f", "", "Follow pid\n")
	var q = flag.Bool("q", false, "Redirect standar input, output, error to /dev/null\n")
	var u = flag.String("u", "", "Execute command on behalf user\n")
	var v = flag.Bool("v", false, fmt.Sprintf("Print version: %s\n", version))
	var c = flag.String("c", "", "run.yml configuration file")

	flag.Usage = func() {
		fmt.Fprintf(os.Stderr, "usage: %s [-qv] [-f pid_file] [-u user] command arguments\n\n", os.Args[0])
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
		if _, err := os.Stat(*c); os.IsNotExist(err) {
			fmt.Printf("Cannot read file: %s, use -h for more info.\n\n", *f)
			os.Exit(1)
		}
	}

	cmd, err := ir.NewDaemon(u, f, q)
	if err != nil {
		fmt.Println(err)
		os.Exit(1)
	}

	err = cmd.Run(flag.Args())
	if err != nil {
		fmt.Println(err)
		os.Exit(1)
	}
}
