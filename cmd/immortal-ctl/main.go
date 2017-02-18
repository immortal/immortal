package main

import (
	"flag"
	"fmt"
	"io/ioutil"
	"log"
	"os"
	"path/filepath"

	"github.com/immortal/immortal"
)

var version string

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
		fmt.Fprintf(os.Stderr, "usage: %s [option] [signal] service\n\n%s\n%s\n%s\n%s\n%s\n%s\n\n%s\n%s\n%s\n%s\n%s\n%s\n%s\n%s\n%s\n%s\n%s\n%s\n%s\n%s\n%s\n\n",
			os.Args[0],
			"  Options:",
			"    start     Start the service",
			"    stop      Stop the service",
			"    restart   Restart the service",
			"    once      Do not restart if service stops",
			"    kill      Terminate service",
			"  Signals:",
			"    a, alrm   ALRM",
			"    c, cont   CONT",
			"    d, down   TERM",
			"    h, hup    HUP",
			"    i, int    INT",
			"    in, TTIN  TTIN",
			"    ou, TTOU  TTOU",
			"    s, stop   STOP",
			"    q, quit   QUIT",
			"    t, term   TERM",
			"    q, quit   QUIT",
			"    1, usr1   USR1",
			"    2, usr2   USR2",
			"    w, winch  WINCH",
		)
		flag.PrintDefaults()
	}

	flag.Parse()

	if *v {
		fmt.Printf("%s\n", version)
		os.Exit(0)
	}

	systemServices, err := ioutil.ReadDir(sdir)
	if err != nil {
		log.Fatal(err)
	}

	format := "%+7v %+10v %+10s   %-10s %-10s\n"
	fmt.Printf(format, "PID", "Up", "Down", "Name", "CMD")
	for _, file := range systemServices {
		if file.IsDir() {
			socket := filepath.Join(sdir, file.Name(), "immortal.sock")
			if _, err := os.Stat(socket); err == nil {
				status := &immortal.Status{}
				if err := immortal.GetJSON(socket, "/", status); err == nil {
					if status.Down != "" {
						fmt.Printf(format, immortal.Red(file.Name()), status.Pid, status.Up, status.Down, "--")
					} else {
						fmt.Printf(format, status.Pid, status.Up, status.Down, immortal.Yellow(fmt.Sprintf("%-10s", file.Name())), status.Cmd)
					}
				} else {
					// clean
					os.RemoveAll(filepath.Join(sdir, file.Name()))
				}
			}
		}
	}
}
