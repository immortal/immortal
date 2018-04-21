package immortal

import (
	"encoding/json"
	"fmt"
	"log"
	"net"
	"net/http"
	"path/filepath"
	"strings"
	"sync/atomic"
	"time"

	"github.com/nbari/violetear"
)

// Status struct
type Status struct {
	Pid    int    `json:"pid"`
	Up     string `json:"up,omitempty"`
	Down   string `json:"down,omitempty"`
	Cmd    string `json:"cmd"`
	Fpid   bool   `json:"fpid"`
	Count  uint32 `json:"count"`
	Status string `json:"status,omitempty"`
	Name   string `json:"name,omitempty"`
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

// HandleStatus return process status
func (d *Daemon) HandleStatus(w http.ResponseWriter, r *http.Request) {
	status := Status{
		Cmd:   strings.Join(d.cfg.command, " "),
		Count: atomic.LoadUint32(&d.count),
	}

	//  only if process is running
	if d.process.cmd != nil {
		status.Fpid = d.fpid
		status.Pid = d.process.Pid()
		if d.process.eTime.IsZero() {
			status.Up = AbsSince(d.process.sTime)
		} else {
			status.Down = AbsSince(d.process.eTime)
		}
	} else {
		startin := d.process.sTime.Add(time.Duration(d.cfg.Wait) * time.Second)
		status.Status = fmt.Sprintf("Starting in %0.1f seconds", startin.Sub(time.Now()).Seconds())
	}
	if len(d.cfg.Name) > 0 {
		status.Name = d.cfg.Name
	}

	// return status in json
	w.Header().Set("Content-Type", "application/json")
	if err := json.NewEncoder(w).Encode(status); err != nil {
		log.Println(err)
	}
}
