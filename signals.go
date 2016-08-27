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
		d.ctrl <- controlUp{}
		//d.Run()

	// d: Down. If the service is running, send it a TERM signal. After it stops, do not restart it.
	case "d", "down":
		d.ctrl <- controlOnce{}
		d.ctrl <- controlSignal{syscall.SIGTERM}

	// o: Once. If the service is not running, start it. Do not restart it if it stops.
	case "o", "once":
		d.ctrl <- controlOnce{}

	// t: Terminate. Send the service a TERM signal.
	case "t", "term":
		d.ctrl <- controlSignal{syscall.SIGTERM}

	// p: Pause. Send the service a STOP signal.
	case "p", "pause", "s", "stop":
		d.ctrl <- controlSignal{syscall.SIGSTOP}

	// c: Continue. Send the service a CONT signal.
	case "c", "cont":
		d.ctrl <- controlSignal{syscall.SIGCONT}

	// h: Hangup. Send the service a HUP signal.
	case "h", "hup":
		d.ctrl <- controlSignal{syscall.SIGHUP}

	// a: Alarm. Send the service an ALRM signal.
	case "a", "alrm":
		d.ctrl <- controlSignal{syscall.SIGALRM}

	// i: Interrupt. Send the service an INT signal.
	case "i", "int":
		d.ctrl <- controlSignal{syscall.SIGINT}

	// q: QUIT. Send the service a QUIT signal.
	case "q", "quit":
		d.ctrl <- controlSignal{syscall.SIGQUIT}

	// 1: USR1. Send the service a USR1 signal.
	case "1", "usr1":
		d.ctrl <- controlSignal{syscall.SIGUSR1}

	// 2: USR2. Send the service a USR2 signal.
	case "2", "usr2":
		d.ctrl <- controlSignal{syscall.SIGUSR2}

	// k: Kill. Send the service a KILL signal.
	case "k", "kill":
		d.ctrl <- controlKill{}

	// in: TTIN. Send the service a TTIN signal.
	case "in", "TTIN":
		d.ctrl <- controlSignal{syscall.SIGTTIN}

	// ou: TTOU. Send the service a TTOU signal.
	case "ou", "out", "TTOU":
		d.ctrl <- controlSignal{syscall.SIGTTOU}

	// w: WINCH. Send the service a WINCH signal.
	case "w", "winch":
		d.ctrl <- controlSignal{syscall.SIGWINCH}

	// x: Exit. supervise will exit as soon as the service is down. If you use this option on a stable system, you're doing something wrong; supervise is designed to run forever.
	case "x", "exit":
		close(d.quit)

	default:
		log.Printf("unknown signal: %s", signal)
		fmt.Fprintf(d.fifo_ok, "%s\n", signal)
	}
}
