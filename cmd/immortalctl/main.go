package main

import (
	"flag"
	"fmt"
	"os"
	"sync"
	"time"

	"github.com/immortal/immortal"
)

var version string

func main() {
	var (
		sdir, serviceName, signal string
		options                   = []string{"exit", "halt", "once", "restart", "restart", "start", "status", "stop"}
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
		asciiOnly                 = flag.Bool("A", false, "Output only ASCII characters")
	)

	// if IMMORTAL_SDIR env is set, use it as default sdir
	if sdir = os.Getenv("IMMORTAL_SDIR"); sdir == "" {
		sdir = "/var/run/immortal"
	}

	// Move all of the "options" to the end of the flags so that flag parses
	// arguments correctly
	for i := 2; i < len(os.Args); i++ {
		if os.Args[i][0] == '-' && os.Args[i-1][0] != '-' {
			os.Args[i], os.Args[i-1] = os.Args[i-1], os.Args[i]
		}
	}

	flag.Usage = func() {
		fmt.Fprintf(os.Stderr, "usage: %s [-v] [option] [signals -12achik,in,ou,qstw] service\n\n%s\n%s\n%s\n%s\n%s\n%s\n%s\n%s\n\n%s\n%s\n%s\n%s\n%s\n%s\n%s\n%s\n%s\n%s\n%s\n%s\n%s\n%s\n\n%s %s\n",
			os.Args[0],
			"  Options:",
			"    exit      Exits only the supervisor, not the service.",
			"    halt      Stop the service by sending a TERM signal, and exits supervisor - restart.",
			"    once      If the service is not running, start it. Do not restart it if it stops.",
			"    start     Start the service.",
			"    status    Print status.",
			"    stop      Stop the service by sending a TERM signal.",
			"    -A        ASCII only output (no terminal control codes)",
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

	switch flag.NArg() {
	case 2:
		signal = flag.Arg(0)
		serviceName = flag.Arg(1)
	case 1:
		signal = flag.Arg(0)
	case 0:
		signal = "status"
	default:
	}

	// Ensure the signal is a valid keyword
	for _, v := range options {
		if signal == v {
			exit = false
			break
		}
	}

	// to avoid collision with signal STOP
	if signal == "stop" {
		signal = "down"
	}
	if signal != "status" && serviceName == "" {
		exit = true
	}

	if exit {
		fmt.Fprintf(os.Stderr, "Invalid arguments, use (\"%s -help\") for help.\n", os.Args[0])
		os.Exit(1)
	}

	// new controller
	ctl := &immortal.Controller{}

	// get status for all services
	services, _ := ctl.FindServices(sdir)

	// get user $HOME/.immortal services
	userSdir, err := immortal.GetUserSdir()
	if err != nil {
		fmt.Fprintf(os.Stderr, err.Error())
		os.Exit(1)
	}
	if userServices, err := ctl.FindServices(userSdir); err == nil {
		services = append(services, userServices...)
	}

	type Pad struct {
		pid, up, down, name int
	}

	queue := make(chan *Pad)

	// apply options/signals to specified services
	wg.Add(len(services))
	for _, service := range services {
		if serviceName != "" {
			if serviceName != "*" {
				if service.Name != serviceName {
					wg.Done()
					continue
				}
			}
		}
		go func(s *immortal.ServiceStatus) {
			var (
				err error
				res *immortal.SignalResponse
			)
			if signal != "status" {
				res, err = ctl.SendSignal(s.Socket, signal)
				if err == nil {
					time.Sleep(time.Millisecond)
				}
			}
			status, err := ctl.GetStatus(s.Socket)
			if err != nil {
				ctl.PurgeServices(s.Socket)
				// mainly for signal exit
				queue <- &Pad{}
			} else {
				s.Status = status
				s.SignalResponse = res
				queue <- &Pad{
					pid:  len(fmt.Sprintf("%d", status.Pid)),
					up:   len(status.Up),
					down: len(status.Down),
					name: len(s.Name),
				}
			}
		}(service)
	}

	go func() {
		for q := range queue {
			if q.pid > ppid {
				ppid = q.pid
			}
			if q.up > pup {
				pup = q.up
			}
			if q.down > pdown {
				pdown = q.down
			}
			if q.name > pname {
				pname = q.name
			}
			wg.Done()
		}
	}()
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
			var name string
			if *asciiOnly {
				name = fmt.Sprintf("%-*s", pname, s.Name)
			} else if s.Status.Fpid {
				name = immortal.Yellow(fmt.Sprintf("%-*s", pname, s.Name))
			} else {
				if s.Status.Down != "" {
					name = immortal.Red(fmt.Sprintf("%-*s", pname, s.Name))
				} else {
					name = immortal.Green(fmt.Sprintf("%-*s", pname, s.Name))
				}
			}
			fmt.Printf(format,
				s.Status.Pid,
				s.Status.Up,
				s.Status.Down,
				name,
				s.Status.Cmd)
			if s.SignalResponse != nil && s.SignalResponse.Err != "" {
				if *asciiOnly {
					println(fmt.Sprintf(" - %s", s.SignalResponse.Err))
				} else {
					println(immortal.Yellow(fmt.Sprintf(" - %s", s.SignalResponse.Err)))
				}
			} else {
				println()
			}
		} else if s.Status.Status != "" {
			// print status about process that will start after the defined WAIT value
			if *asciiOnly {
				fmt.Printf(format,
					"",
					"",
					"",
					fmt.Sprintf("%-*s", pname, s.Name),
					fmt.Sprintf("%s - %s\n", s.Status.Cmd, s.Status.Status))
			} else {
				fmt.Printf(format,
					"",
					"",
					"",
					immortal.Green(fmt.Sprintf("%-*s", pname, s.Name)),
					immortal.Yellow(fmt.Sprintf("%s - %s\n", s.Status.Cmd, s.Status.Status)))
			}
		}
	}
}
