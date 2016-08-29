package immortal

import (
	"fmt"
	"log"
	"syscall"
)

func (s *Sup) HandleSignals(signal string, d *Daemon) {
	switch signal {
	// u: Up. If the service is not running, start it. If the service stops, restart it.
	case "u", "up":
		d.lock = 0
		d.lock_once = 0

	// d: Down. If the service is running, send it a TERM signal. After it stops, do not restart it.
	case "d", "down":
		d.lock_once = 1
		s.process.Signal(syscall.SIGTERM)

	// o: Once. If the service is not running, start it. Do not restart it if it stops.
	case "o", "once":
		d.lock_once = 1

	// t: Terminate. Send the service a TERM signal.
	case "t", "term":
		s.process.Signal(syscall.SIGTERM)

	// p: Pause. Send the service a STOP signal.
	case "p", "pause", "s", "stop":
		s.process.Signal(syscall.SIGSTOP)

	// c: Continue. Send the service a CONT signal.
	case "c", "cont":
		s.process.Signal(syscall.SIGCONT)

	// h: Hangup. Send the service a HUP signal.
	case "h", "hup":
		s.process.Signal(syscall.SIGHUP)

	// a: Alarm. Send the service an ALRM signal.
	case "a", "alrm":
		s.process.Signal(syscall.SIGALRM)

	// i: Interrupt. Send the service an INT signal.
	case "i", "int":
		s.process.Signal(syscall.SIGINT)

	// q: QUIT. Send the service a QUIT signal.
	case "q", "quit":
		s.process.Signal(syscall.SIGQUIT)

	// 1: USR1. Send the service a USR1 signal.
	case "1", "usr1":
		s.process.Signal(syscall.SIGUSR1)

	// 2: USR2. Send the service a USR2 signal.
	case "2", "usr2":
		s.process.Signal(syscall.SIGUSR2)

	// k: Kill. Send the service a KILL signal.
	case "k", "kill":
		if err := s.process.Kill(); err != nil {
			log.Print(err)
		}

	// in: TTIN. Send the service a TTIN signal.
	case "in", "TTIN":
		s.process.Signal(syscall.SIGTTIN)

	// ou: TTOU. Send the service a TTOU signal.
	case "ou", "out", "TTOU":
		s.process.Signal(syscall.SIGTTOU)

	// w: WINCH. Send the service a WINCH signal.
	case "w", "winch":
		s.process.Signal(syscall.SIGWINCH)

	// x: Exit. supervise will exit as soon as the service is down. If you use this option on a stable system, you're doing something wrong; supervise is designed to run forever.
	case "x", "exit":
		close(d.quit)

	default:
		log.Printf("unknown signal: %s", signal)
		fmt.Fprintf(d.fifo_ok, "%s\n", signal)
	}
}
