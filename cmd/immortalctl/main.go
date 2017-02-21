package main

import (
	"flag"
	"fmt"
	"os"
	"os/user"
	"path/filepath"

	"github.com/immortal/immortal"
)

var version string

const FORMAT = "%+7v %+10v %+10s   %-10s %-10s\n"

// immortal-ctl options service
// immortal-ctl status (print status of all services)
func main() {
	var (
		v    = flag.Bool("v", false, fmt.Sprintf("Print version: %s", version))
		sdir string
	)

	// if IMMORTAL_SDIR env is set, use it as default sdir
	if sdir = os.Getenv("IMMORTAL_SDIR"); sdir == "" {
		sdir = "/var/run/immortal"
	}

	flag.Usage = func() {
		fmt.Fprintf(os.Stderr, "usage: %s [option] [signal] service\n\n%s\n%s\n%s\n%s\n%s\n%s\n%s\n\n%s\n%s\n%s\n%s\n%s\n%s\n%s\n%s\n%s\n%s\n%s\n%s\n%s\n%s\n%s\n\n",
			os.Args[0],
			"  Options:",
			"    kill      Terminate service",
			"    once      Do not restart if service stops",
			"    restart   Restart the service",
			"    start     Start the service",
			"    status    Print status of services",
			"    stop      Stop the service",
			"  Signals:",
			"    a         ALRM",
			"    c         CONT",
			"    d         TERM",
			"    h         HUP",
			"    i         INT",
			"    in        TTIN",
			"    ou        TTOU",
			"    s         STOP",
			"    q         QUIT",
			"    t         TERM",
			"    q         QUIT",
			"    1         USR1",
			"    2         USR2",
			"    w         WINCH",
		)
		flag.PrintDefaults()
	}

	flag.Parse()

	if *v {
		fmt.Printf("%s\n", version)
		os.Exit(0)
	}

	// get status for all services
	services, _ := immortal.FindServices(sdir)

	// get user $HOME/.immortal services
	if usr, err := user.Current(); err == nil {
		if userServices, err := immortal.FindServices(
			filepath.Join(usr.HomeDir, ".immortal"),
		); err == nil {
			services = append(services, userServices...)
		}
	}

	if len(services) > 0 {
		fmt.Printf(FORMAT, "PID", "Up", "Down", "Name", "CMD")
		for _, s := range services {
			status, err := immortal.GetStatus(s[1])
			if err != nil {
				immortal.PurgeServices(s[1])
			} else {
				if status.Down != "" {
					fmt.Printf(FORMAT, status.Pid, status.Up, status.Down, immortal.Red(fmt.Sprintf("%-10s", s[0])), status.Cmd)
				} else {
					fmt.Printf(FORMAT, status.Pid, status.Up, status.Down, immortal.Green(fmt.Sprintf("%-10s", s[0])), status.Cmd)
				}
			}
		}
	} else {
		println("No services found")
	}
}
