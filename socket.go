package immortal

import (
	"encoding/json"
	"log"
	"net"
	"net/http"
	"path/filepath"
	"strings"

	"github.com/nbari/violetear"
)

type Status struct {
	Pid  int    `json:"pid"`
	Up   string `json:"up,omitempty"`
	Down string `json:"down,omitempty"`
	Cmd  string `json:"cmd"`
}

// Listen creates a unix socket used for control the daemon
func (d *Daemon) Listen() error {
	l, err := net.Listen("unix", filepath.Join(d.supDir, "immortal.sock"))
	if err != nil {
		return err
	}
	router := violetear.New()
	router.Verbose = false
	router.HandleFunc("/", d.HandleStatus)
	router.HandleFunc("/signal/*", d.HandleSignal)
	go http.Serve(l, router)
	return nil
}

// Status return process status
func (d *Daemon) HandleStatus(w http.ResponseWriter, r *http.Request) {
	status := Status{
		Pid: d.process.Pid(),
		Cmd: strings.Join(d.cfg.command, " "),
	}
	if d.process.eTime.IsZero() {
		status.Up = AbsSince(d.process.sTime)
	} else {
		status.Down = AbsSince(d.process.eTime)
	}
	if err := json.NewEncoder(w).Encode(status); err != nil {
		log.Println(err)
	}
}
