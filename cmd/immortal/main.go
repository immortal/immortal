package main

import (
	"flag"
	"fmt"
	"github.com/immortal/immortal"
	"github.com/immortal/natcasesort"
	"os"
	"sort"
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
		var flags []string
		fs.VisitAll(func(f *flag.Flag) {
			flags = append(flags, f.Name)
		})
		sort.Sort(natcasesort.Sort(flags))
		for _, v := range flags {
			f := fs.Lookup(v)
			s := fmt.Sprintf("  -%s", f.Name)
			name, usage := flag.UnquoteUsage(f)
			if len(name) > 0 {
				s += " " + name
			}
			if len(s) <= 4 {
				s += "\t"
			} else {
				s += "\n    \t"
			}
			s += usage
			fmt.Println(s)
		}
	}

	flags, err := immortal.ParseArgs(parser, fs)
	if err != nil {
		fmt.Fprintln(os.Stderr, err)
		os.Exit(1)
	}

	// if -v print version
	if flags.Version {
		fmt.Printf("%s\n", version)
		os.Exit(0)
	}

	fmt.Println(fs.Args())
}
