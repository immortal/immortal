package immortal

import (
	"fmt"
	"log"
	"syscall"
)

// HandleSignals send signals to the current process
func (s *Sup) HandleSignals(signal string, d *Daemon) {
	var err error
	switch signal {
	// a: Alarm. Send the service an ALRM signal.
	case "a", "alrm":
		err = s.process.Signal(syscall.SIGALRM)

	// c: Continue. Send the service a CONT signal.
	case "c", "cont":
		err = s.process.Signal(syscall.SIGCONT)

	// d: Down. If the service is running, send it a TERM signal. After it stops, do not restart it.
	case "d", "down":
		d.lockOnce = 1
		err = s.process.Signal(syscall.SIGTERM)

	// h: Hangup. Send the service a HUP signal.
	case "h", "hup":
		err = s.process.Signal(syscall.SIGHUP)

	// i: Interrupt. Send the service an INT signal.
	case "i", "int":
		err = s.process.Signal(syscall.SIGINT)

	// in: TTIN. Send the service a TTIN signal.
	case "in", "TTIN":
		err = s.process.Signal(syscall.SIGTTIN)

	// k: Kill. Send the service a KILL signal.
	case "k", "kill":
		err = s.process.Kill()

	// o: Once. If the service is not running, start it. Do not restart it if it stops.
	case "o", "once":
		d.lockOnce = 1

	// ou: TTOU. Send the service a TTOU signal.
	case "ou", "out", "TTOU":
		err = s.process.Signal(syscall.SIGTTOU)

	// p: Pause. Send the service a STOP signal.
	case "p", "pause", "s", "stop":
		err = s.process.Signal(syscall.SIGSTOP)

	// q: QUIT. Send the service a QUIT signal.
	case "q", "quit":
		err = s.process.Signal(syscall.SIGQUIT)

	// t: Terminate. Send the service a TERM signal.
	case "t", "term":
		err = s.process.Signal(syscall.SIGTERM)

	// u: Up. If the service is not running, start it. If the service stops, restart it.
	case "u", "up":
		d.lock = 0
		d.lockOnce = 0

	// 1: USR1. Send the service a USR1 signal.
	case "1", "usr1":
		err = s.process.Signal(syscall.SIGUSR1)

	// 2: USR2. Send the service a USR2 signal.
	case "2", "usr2":
		err = s.process.Signal(syscall.SIGUSR2)

	// w: WINCH. Send the service a WINCH signal.
	case "w", "winch":
		err = s.process.Signal(syscall.SIGWINCH)

	// x: Exit. supervise will exit as soon as the service is down. If you use this option on a stable system, you're doing something wrong; supervise is designed to run forever.
	case "x", "exit":
		close(d.quit)

	default:
		log.Printf("Unknown signal: %s", signal)
		fmt.Fprintf(d.fifoOk, "%s\n", signal)
	}

	if err != nil {
		log.Print(err)
	}
}
