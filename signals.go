package immortal

import (
	"encoding/json"
	"fmt"
	"log"
	"net/http"
	"syscall"

	"github.com/nbari/violetear"
)

// HandleSignals send signals to the current process
func (d *Daemon) HandleSignal(w http.ResponseWriter, r *http.Request) {
	var err error

	// get signal from request params
	params := r.Context().Value(violetear.ParamsKey).(violetear.Params)
	signal := params["*"].(string)

	switch signal {
	// a: Alarm. Send the service an ALRM signal.
	case "a", "alrm":
		err = d.process.Signal(syscall.SIGALRM)

	// c: Continue. Send the service a CONT signal.
	case "c", "cont":
		err = d.process.Signal(syscall.SIGCONT)

	// d: Down. If the service is running, send it a TERM signal. After it stops, do not restart it.
	case "d", "down":
		d.lockOnce = 1
		err = d.process.Signal(syscall.SIGTERM)

	// h: Hangup. Send the service a HUP signal.
	case "h", "hup":
		err = d.process.Signal(syscall.SIGHUP)

	// i: Interrupt. Send the service an INT signal.
	case "i", "int":
		err = d.process.Signal(syscall.SIGINT)

	// in: TTIN. Send the service a TTIN signal.
	case "in", "TTIN":
		err = d.process.Signal(syscall.SIGTTIN)

	// k: Kill. Send the service a KILL signal.
	case "k", "kill":
		err = d.process.Kill()

	// o: Once. If the service is not running, start it. Do not restart it if it stops.
	case "o", "once":
		d.lockOnce = 1

	// ou: TTOU. Send the service a TTOU signal.
	case "ou", "out", "TTOU":
		err = d.process.Signal(syscall.SIGTTOU)

	// p: Pause. Send the service a STOP signal.
	case "p", "pause", "s", "stop":
		err = d.process.Signal(syscall.SIGSTOP)

	// q: QUIT. Send the service a QUIT signal.
	case "q", "quit":
		err = d.process.Signal(syscall.SIGQUIT)

	// t: Terminate. Send the service a TERM signal.
	case "t", "term":
		err = d.process.Signal(syscall.SIGTERM)

	// u: Up. If the service is not running, start it. If the service stops, restart it.
	case "u", "up":
		d.lock = 0
		d.lockOnce = 0

	// 1: USR1. Send the service a USR1 signal.
	case "1", "usr1":
		err = d.process.Signal(syscall.SIGUSR1)

	// 2: USR2. Send the service a USR2 signal.
	case "2", "usr2":
		err = d.process.Signal(syscall.SIGUSR2)

	// w: WINCH. Send the service a WINCH signal.
	case "w", "winch":
		err = d.process.Signal(syscall.SIGWINCH)

	// x: Exit. If you use this option on a stable system, you're doing something wrong; supervise is designed to run forever.
	case "x", "exit":
		close(d.quit)

	default:
		err = fmt.Errorf("Unknown signal: %s", signal)
	}

	// Response struct to return the error in json format
	type Response struct {
		Err string
	}

	res := &Response{}

	if err != nil {
		res.Err = err.Error()
		log.Print(err)
	}

	// return the error on the Response json encoded
	if err := json.NewEncoder(w).Encode(res); err != nil {
		log.Println(err)
	}
}
