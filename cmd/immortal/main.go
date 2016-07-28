package main

import (
	"flag"
	"fmt"
	ir "github.com/immortal/immortal"
	"os"
	"os/user"
)

var version string

func main() {
	cfg := ir.New()
	cfg.Parser.Parse(&cfg.Flags)

	//	fmt.Printf("%#v", cfg)

	// print version
	if cfg.Version {
		fmt.Printf("%s\n", version)
		os.Exit(0)
	}

	// if no args exit
	if len(flag.Args()) < 1 {
		fmt.Fprintf(os.Stderr, "Missing command, use (\"%s -h\") for help.\n", os.Args[0])
		os.Exit(1)
	}

	// check if -d exists
	if cfg.Wrkdir != "" {
		if !cfg.Exists(cfg.Wrkdir) {
			fmt.Printf("-d %s does not exist or has wrong permissions.\n", cfg.Wrkdir)
			os.Exit(1)
		}
	}

	// check if -u is set
	if cfg.Flags.User != "" {
		usr, err := cfg.Users.Lookup(cfg.User)
		if err != nil {
			if _, ok := err.(user.UnknownUserError); ok {
				fmt.Printf("User %s does not exist.\n", cfg.User)
			} else if err != nil {
				fmt.Printf("Error looking up user: %s\n", cfg.User)
			}
			os.Exit(1)
		}
		println(usr)
	}

}
