package immortal

import (
	"encoding/json"
	"fmt"
	"log"
	"net"
	"net/http"
	"path/filepath"
	"time"

	"github.com/nbari/violetear"
)

type Status struct {
	Pid  int    `json:"pid"`
	Up   string `json:"up,omitempty"`
	Down string `json:"down,omitempty"`
}

// Listen creates a unix socket used for control the daemon
func (d *Daemon) Listen() error {
	l, err := net.Listen("unix", filepath.Join(d.supDir, "immortal.sock"))
	if err != nil {
		return err
	}
	router := violetear.New()
	router.Verbose = false
	router.HandleFunc("/", d.Status)
	go http.Serve(l, router)
	return nil
}

// Status return process status
func (d *Daemon) Status(w http.ResponseWriter, r *http.Request) {
	status := Status{
		Pid: d.process.Pid(),
	}
	if d.process.eTime.IsZero() {
		status.Up = fmt.Sprintf("%s", time.Since(d.process.sTime))
	} else {
		status.Down = fmt.Sprintf("%s", time.Since(d.process.eTime))
	}
	if err := json.NewEncoder(w).Encode(status); err != nil {
		log.Println(err)
	}
}
