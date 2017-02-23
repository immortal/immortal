package main

import (
	"flag"
	"fmt"
	"os"
	"os/user"
	"path/filepath"
	"sort"
	"sync"

	"github.com/immortal/immortal"
)

var version string

const FORMAT = "%+7v %+15s %+15s   %-10s %-10s\n"

// immortal-ctl options service
// immortal-ctl status (print status of all services)
func main() {
	var (
		v    = flag.Bool("v", false, fmt.Sprintf("Print version: %s", version))
		sdir string
		wg   sync.WaitGroup
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

	var ppid, pup, pdown, pname, pcmd []int
	if len(services) > 0 {
		wg.Add(len(services))
		for _, service := range services {
			go func(s *immortal.ServiceStatus, ppid, pup, pdown, pname, pcmd []int) {
				defer wg.Done()
				status, err := immortal.GetStatus(s.Socket)
				if err != nil {
					immortal.PurgeServices(s.Socket)
				} else {
					s.Status = status
					ppid = append(ppid, len(fmt.Sprintf("%d", status.Pid)))
					pup = append(pup, len(status.Up))
					pdown = append(pdown, len(status.Down))
					pname = append(pname, len(s.Name))
					pcmd = append(pcmd, len(status.Cmd))
				}
			}(service, ppid, pup, pdown, pname, pcmd)
		}
	} else {
		println("No services found")
		return
	}

	wg.Wait()

	// for padding and prety print the status
	sort.Ints(ppid)
	sort.Ints(pup)
	sort.Ints(pdown)
	sort.Ints(pname)
	sort.Ints(pcmd)

	format := fmt.Sprintf("%%+%dv  %%+%ds  %%+%ds  %%-%ds  %%-%ds\n",
		ppid[len(ppid)-1],
		pup[len(pup)-1],
		pdown[(len(pdown)+1)-1],
		pname[len(pname)-1],
		pcmd[len(pcmd)-1])

	fmt.Printf("format = %+v\n", format)
	fmt.Printf("ppid = %+v\n", ppid)

	fmt.Printf(format, "PID", "Up", "Down", "Name", "CMD")
	for _, s := range services {
		if s.Status.Down != "" {
			fmt.Printf(format, s.Status.Pid, s.Status.Up, s.Status.Down, immortal.Red(fmt.Sprintf("%-*s", pname[len(pname)-1], s.Name)), s.Status.Cmd)
		} else {
			fmt.Printf(format, s.Status.Pid, s.Status.Up, s.Status.Down, immortal.Green(fmt.Sprintf("%-*s", pname[len(pname)-1], s.Name)), s.Status.Cmd)
		}
	}

}
