package immortal

import (
	"encoding/json"
	"fmt"
	"log"
	"net/http"
	"syscall"

	"github.com/nbari/violetear"
)

// SignalResponse struct to return the error in json format
type SignalResponse struct {
	Err string
}

// HandleSignal send signals to the current process
func (d *Daemon) HandleSignal(w http.ResponseWriter, r *http.Request) {
	var err error

	// get signal from request params
	params := r.Context().Value(violetear.ParamsKey).(violetear.Params)
	signal := params["*"].(string)

	switch signal {
	// a: Alarm. Send the service an ALRM signal.
	case "a", "alrm", "ALRM":
		err = d.process.Signal(syscall.SIGALRM)

	// c: Continue. Send the service a CONT signal.
	case "c", "cont", "CONT":
		err = d.process.Signal(syscall.SIGCONT)

	// d: Down. If the service is running, send it a TERM signal. After it stops, do not restart it.
	case "d", "down":
		d.lockOnce = 1
		err = d.process.Signal(syscall.SIGTERM)

	// h: Hangup. Send the service a HUP signal.
	case "h", "hup", "HUP":
		err = d.process.Signal(syscall.SIGHUP)

	// i: Interrupt. Send the service an INT signal.
	case "i", "int", "INT":
		err = d.process.Signal(syscall.SIGINT)

	// in: TTIN. Send the service a TTIN signal.
	case "in", "ttin", "TTIN":
		err = d.process.Signal(syscall.SIGTTIN)

		// k: Kill. Send the service a KILL signal.
	case "k", "kill", "KILL":
		if d.fpid {
			err = d.process.Signal(syscall.SIGKILL)
		} else {
			err = d.process.Kill()
		}

	// o: Once. If the service is not running, start it. Do not restart it if it stops.
	case "o", "once":
		d.lockOnce = 1
		if !d.IsRunning(d.process.Pid()) {
			d.lock = 0
			d.run <- struct{}{}
		}

	// ou: TTOU. Send the service a TTOU signal.
	case "ou", "ttou", "TTOU":
		err = d.process.Signal(syscall.SIGTTOU)

	// s: stop. Send the service a STOP signal.
	case "s", "stop", "STOP":
		err = d.process.Signal(syscall.SIGSTOP)

	// q: QUIT. Send the service a QUIT signal.
	case "q", "quit", "QUIT":
		err = d.process.Signal(syscall.SIGQUIT)

	// t: Terminate. Send the service a TERM signal.
	case "t", "term", "TERM":
		err = d.process.Signal(syscall.SIGTERM)

	// u: Up. If the service is not running, start it. If the service stops, restart it.
	case "u", "up", "start":
		d.lockOnce = 0
		if !d.IsRunning(d.process.Pid()) {
			d.lock = 0
		}
		d.run <- struct{}{}

	// 1: USR1. Send the service a USR1 signal.
	case "1", "usr1", "USR1":
		err = d.process.Signal(syscall.SIGUSR1)

	// 2: USR2. Send the service a USR2 signal.
	case "2", "usr2", "USR2":
		err = d.process.Signal(syscall.SIGUSR2)

	// w: WINCH. Send the service a WINCH signal.
	case "w", "winch", "WINCH":
		err = d.process.Signal(syscall.SIGWINCH)

	// x: Exit. If you use this option on a stable system, you're doing something wrong; supervise is designed to run forever.
	case "x", "exit":
		close(d.quit)

	default:
		err = fmt.Errorf("Unknown signal: %s", signal)
	}

	res := &SignalResponse{}
	if err != nil {
		res.Err = err.Error()
		log.Print(err)
	}

	// return the error on the Response json encoded
	if err := json.NewEncoder(w).Encode(res); err != nil {
		log.Println(err)
	}
}
