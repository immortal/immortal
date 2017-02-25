package main

import (
	"flag"
	"fmt"
	"os"
	"os/user"
	"path/filepath"
	"sync"

	"github.com/immortal/immortal"
)

var version string

func exit1(err error) {
	fmt.Println(err)
	os.Exit(1)
}

// immortal-ctl options service
// immortal-ctl status (print status of all services)
func main() {
	var (
		options                 = []string{"kill", "one", "restart", "start", "status", "stop"}
		ppid, pup, pdown, pname int
		sdir                    string
		v                       = flag.Bool("v", false, fmt.Sprintf("Print version: %s", version))
		wg                      sync.WaitGroup
	)

	// if IMMORTAL_SDIR env is set, use it as default sdir
	if sdir = os.Getenv("IMMORTAL_SDIR"); sdir == "" {
		sdir = "/var/run/immortal"
	}

	// if no options defaults to status
	if len(os.Args) == 1 {
		os.Args = append(os.Args, "status")
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
			"    signals:",
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

	// check options
	exit := true
	if flag.Arg(0) != "" {
		for _, v := range options {
			if flag.Arg(0) == v {
				exit = false
				break
			}
		}
	}
	if exit {
		exit1(fmt.Errorf("Invalid option, use (\"%s -h\") for help.\n", os.Args[0]))
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

	wg.Add(len(services))
	for _, service := range services {
		go func(s *immortal.ServiceStatus) {
			defer wg.Done()
			status, err := immortal.GetStatus(s.Socket)
			if err != nil {
				immortal.PurgeServices(s.Socket)
			} else {
				s.Status = status
				if l := len(fmt.Sprintf("%d", status.Pid)); l > ppid {
					ppid = l
				}
				if l := len(status.Up); l > pup {
					pup = l
				}
				if l := len(status.Down); l > pdown {
					pdown = l
				}
				if l := len(s.Name); l > pname {
					pname = l
				}
			}
		}(service)
	}
	wg.Wait()

	// format the output
	if ppid < 3 {
		ppid = 3
	}
	if pup < 2 {
		pup = 2
	}
	if pdown < 4 {
		pdown = 4
	}
	format := fmt.Sprintf("%%+%dv   %%+%ds   %%+%ds   %%-%ds   %%s\n",
		ppid,
		pup,
		pdown,
		pname,
	)

	fmt.Printf(format, "PID", "Up", "Down", "Name", "CMD")
	for _, s := range services {
		if s.Status.Pid > 0 {
			if s.Status.Down != "" {
				fmt.Printf(format, s.Status.Pid, s.Status.Up, s.Status.Down, immortal.Red(fmt.Sprintf("%-*s", pname, s.Name)), s.Status.Cmd)
			} else {
				fmt.Printf(format, s.Status.Pid, s.Status.Up, s.Status.Down, immortal.Green(fmt.Sprintf("%-*s", pname, s.Name)), s.Status.Cmd)
			}
		}
	}
}
