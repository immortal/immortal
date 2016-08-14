package immortal

import (
	"fmt"
	"log"
	"syscall"
)

func (self *Sup) HandleSignals(signal string, d *Daemon) {
	switch signal {
	// u: Up. If the service is not running, start it. If the service stops, restart it.
	case "u", "up":
		if !d.Running() {
			d.lock = 0
			d.Control.state <- fmt.Errorf("UP")
		}
		d.lock_defer = 0

	// d: Down. If the service is running, send it a TERM signal. After it stops, do not restart it.
	case "d", "down":
		d.lock_defer = 1
		if err := d.process.Signal(syscall.SIGTERM); err != nil {
			log.Print(err)
		}

	// t: Terminate. Send the service a TERM signal.
	case "t", "term":
		if err := d.process.Signal(syscall.SIGTERM); err != nil {
			log.Print(err)
		}

	// o: Once. If the service is not running, start it. Do not restart it if it stops.
	case "o", "once":
		d.lock_defer = 1

	// p: Pause. Send the service a STOP signal.
	case "p", "pause", "s", "stop":
		if err := d.process.Signal(syscall.SIGSTOP); err != nil {
			log.Print(err)
		}

	// c: Continue. Send the service a CONT signal.
	case "c", "cont":
		if err := d.process.Signal(syscall.SIGCONT); err != nil {
			log.Print(err)
		}

	// h: Hangup. Send the service a HUP signal.
	case "h", "hup":
		if err := d.process.Signal(syscall.SIGHUP); err != nil {
			log.Print(err)
		}

	// a: Alarm. Send the service an ALRM signal.
	case "a", "alrm":
		if err := d.process.Signal(syscall.SIGALRM); err != nil {
			log.Print(err)
		}

	// i: Interrupt. Send the service an INT signal.
	case "i", "int":
		if err := d.process.Signal(syscall.SIGINT); err != nil {
			log.Print(err)
		}

	// q: QUIT. Send the service a QUIT signal.
	case "q", "quit":
		if err := d.process.Signal(syscall.SIGQUIT); err != nil {
			log.Print(err)
		}

	// 1: USR1. Send the service a USR1 signal.
	case "1", "usr1":
		if err := d.process.Signal(syscall.SIGUSR1); err != nil {
			log.Print(err)
		}

	// 2: USR2. Send the service a USR2 signal.
	case "2", "usr2":
		if err := d.process.Signal(syscall.SIGUSR2); err != nil {
			log.Print(err)
		}

	// k: Kill. Send the service a KILL signal.
	case "k", "kill":
		if err := d.process.Kill(); err != nil {
			log.Print(err)
		}
		// to handle zombies
		//var w syscall.WaitStatus
		//pid, err := syscall.Wait4(-1, &w, 0, nil)
		//if err != nil {
		//log.Println(err)
		//} else {
		//log.Println("pid", pid, "exited", w.Exited(), "exit status", w.ExitStatus())
		//}

	// in: TTIN. Send the service a TTIN signal.
	case "in", "TTIN":
		if err := d.process.Signal(syscall.SIGTTIN); err != nil {
			log.Print(err)
		}

	// ou: TTOU. Send the service a TTOU signal.
	case "ou", "out", "TTOU":
		if err := d.process.Signal(syscall.SIGTTOU); err != nil {
			log.Print(err)
		}

	// w: WINCH. Send the service a WINCH signal.
	case "w", "winch":
		if err := d.process.Signal(syscall.SIGWINCH); err != nil {
			log.Print(err)
		}

	// x: Exit. supervise will exit as soon as the service is down. If you use this option on a stable system, you're doing something wrong; supervise is designed to run forever.
	case "x", "exit":
		close(d.Control.quit)

	case "status", "info":
		log.Print("status, info: ")

	default:
		log.Printf("unknown signal: %s", signal)
		fmt.Fprintf(d.Control.fifo_ok, "%s\n", signal)
	}
}
