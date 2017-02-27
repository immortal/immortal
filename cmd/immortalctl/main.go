package main

import (
	"flag"
	"fmt"
	"os"
	"os/user"
	"path/filepath"
	"strings"
	"sync"
	"time"

	"github.com/immortal/immortal"
)

var version string

func exit1(err error) {
	fmt.Println(err)
	os.Exit(1)
}

func main() {
	var (
		sdir, serviceName, signal string
		options                   = []string{"kill", "once", "restart", "start", "status", "stop", "exit"}
		ppid, pup, pdown, pname   int
		wg                        sync.WaitGroup
		v                         = flag.Bool("v", false, fmt.Sprintf("Print version: %s", version))
		a                         = flag.Bool("a", false, "ALRM")
		c                         = flag.Bool("c", false, "CONT")
		h                         = flag.Bool("h", false, "HUP")
		i                         = flag.Bool("i", false, "INT")
		in                        = flag.Bool("in", false, "TTIN")
		k                         = flag.Bool("k", false, "KILL")
		ou                        = flag.Bool("ou", false, "TTOU")
		q                         = flag.Bool("q", false, "QUIT")
		s                         = flag.Bool("s", false, "STOP")
		t                         = flag.Bool("t", false, "TERM")
		usr1                      = flag.Bool("1", false, "USR1")
		usr2                      = flag.Bool("2", false, "USR2")
		w                         = flag.Bool("w", false, "WINCH")
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
		fmt.Fprintf(os.Stderr, "usage: %s [option] [-12achikinouqstvw] service\n\n%s\n%s\n%s\n%s\n%s\n%s\n%s\n%s\n\n%s\n%s\n%s\n%s\n%s\n%s\n%s\n%s\n%s\n%s\n%s\n%s\n%s\n%s\n\n%s %s\n",
			os.Args[0],
			"  Options:",
			"    exit      Stop service and supervisor",
			"    kill      Terminate service",
			"    once      Do not restart if service stops",
			"    restart   Restart the service",
			"    start     Start the service",
			"    status    Print status",
			"    stop      Stop the service",
			"  Signals:",
			"    -1        USR1",
			"    -2        USR2",
			"    -a        ALRM",
			"    -c        CONT",
			"    -h        HUP",
			"    -i        INT",
			"    -k        KILL",
			"    -in       TTIN",
			"    -ou       TTOU",
			"    -q        QUIT",
			"    -s        STOP",
			"    -t        TERM",
			"    -w        WINCH",
			"  version", version)
	}

	flag.Parse()

	if *v {
		fmt.Printf("%s\n", version)
		os.Exit(0)
	}

	// set signal
	switch true {
	case *a:
		signal = "ALRM"
	case *c:
		signal = "CONT"
	case *h:
		signal = "HUP"
	case *i:
		signal = "INT"
	case *k:
		signal = "KILL"
	case *in:
		signal = "TTIN"
	case *ou:
		signal = "TTOU"
	case *q:
		signal = "QUIT"
	case *s:
		signal = "STOP"
	case *t:
		signal = "TERM"
	case *w:
		signal = "WINCH"
	case *usr1:
		signal = "USR1"
	case *usr2:
		signal = "USR2"
	}

	// check options and flags
	exit := true
	if flag.NFlag() == 0 && flag.Arg(0) != "" {
		for _, v := range options {
			if flag.Arg(0) == v {
				exit = false
				signal = flag.Arg(0)
				if flag.NArg() == 2 {
					serviceName = flag.Arg(1)
				}
				if signal != "status" && flag.NArg() < 2 {
					exit = true
				}
				break
			}
		}
	} else if flag.NFlag() == 1 && flag.NArg() == 1 {
		serviceName = flag.Arg(0)
		exit = false
	}
	if exit {
		exit1(fmt.Errorf("Invalid arguments, use (\"%s -help\") for help.\n", os.Args[0]))
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

	// apply options/signals to specified services
	wg.Add(len(services))
	for _, service := range services {
		if serviceName != "" {
			if !strings.HasPrefix(service.Name, serviceName) {
				wg.Done()
				continue
			}
		}
		//case "exit", "kill", "once", "restart", "start", "stop":
		go func(s *immortal.ServiceStatus) {
			defer wg.Done()
			var (
				err error
				res *immortal.SignalResponse
			)
			if signal != "status" {
				res, err = immortal.SendSignal(s.Socket, signal)
				if err == nil {
					time.Sleep(time.Millisecond)
				}
			}
			status, err := immortal.GetStatus(s.Socket)
			if err != nil {
				immortal.PurgeServices(s.Socket)
			} else {
				s.Status = status
				s.SignalResponse = res
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
	format := fmt.Sprintf("%%+%dv   %%+%ds   %%+%ds   %%-%ds   %%s",
		ppid,
		pup,
		pdown,
		pname,
	)

	fmt.Printf(format+"\n", "PID", "Up", "Down", "Name", "CMD")
	for _, s := range services {
		if s.Status.Pid > 0 {
			if s.Status.Down != "" {
				fmt.Printf(format,
					s.Status.Pid,
					s.Status.Up,
					s.Status.Down,
					immortal.Red(fmt.Sprintf("%-*s", pname, s.Name)),
					s.Status.Cmd)
			} else {
				fmt.Printf(format,
					s.Status.Pid,
					s.Status.Up,
					s.Status.Down,
					immortal.Green(fmt.Sprintf("%-*s", pname, s.Name)),
					s.Status.Cmd)
			}
			if s.SignalResponse != nil && s.SignalResponse.Err != "" {
				println(immortal.Yellow(fmt.Sprintf(" - %s", s.SignalResponse.Err)))
			} else {
				println()
			}
		}
	}
}
