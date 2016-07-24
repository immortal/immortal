package immortal

import (
	"fmt"
	"log"
)

func (self *Daemon) handleSignals(signal string) {
	fmt.Fprintf(self.ctrl.status_fifo, "pong: %s\n", signal)
	switch signal {
	// -u: Up. If the service is not running, start it. If the service stops, restart it.
	case "u", "up":
		log.Print("u")

	// -d: Down. If the service is running, send it a TERM signal and then a CONT signal. After it stops, do not restart it.
	case "d", "down":
		log.Print("d")

	// -o: Once. If the service is not running, start it. Do not restart it if it stops.
	case "o", "once":
		log.Print("o")

	// -p: Pause. Send the service a STOP signal.
	case "p", "pause", "s", "stop":
		log.Print("p, s")

	// -c: Continue. Send the service a CONT signal.
	case "c", "cont":
		log.Print("c")

	// -h: Hangup. Send the service a HUP signal.
	case "h", "hup":
		log.Print("h")

	// -a: Alarm. Send the service an ALRM signal.
	case "a", "alrm":
		log.Print("a")

	// -i: Interrupt. Send the service an INT signal.
	case "i", "int":
		log.Print("i")

	// -q: QUIT. Send the service a QUIT signal.
	case "q", "quit":
		log.Print("i")

	// -1: USR1. Send the service a USR1 signal.
	case "1", "usr1":
		log.Print("1")

	// -2: USR2. Send the service a USR2 signal.
	case "2", "usr2":
		log.Print("2")

	// -t: Terminate. Send the service a TERM signal.
	case "t", "term":
		log.Print("t")

	// -k: Kill. Send the service a KILL signal.
	case "k", "kill":
		log.Print("k")

	// -in: TTIN. Send the service a TTIN signal.
	case "in", "TTIN":
		log.Print("in")

	// -ou: TTOU. Send the service a TTOU signal.
	case "ou", "out", "TTOU":
		log.Print("ou")

	// -w: WINCH. Send the service a WINCH signal.
	case "w", "winch":
		log.Print("w")

	// -x: Exit. supervise will exit as soon as the service is down. If you use this option on a stable system, you're doing something wrong; supervise is designed to run forever.
	case "x", "exit":
		log.Print("x")

	case "status", "info":
		log.Print("status, info")
	}
}
