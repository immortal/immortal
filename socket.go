package immortal

import (
	"encoding/json"
	"fmt"
	"log"
	"net"
	"net/http"
	"path/filepath"
	"strings"
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
	Count  int    `json:"count"`
	Status string `json:"status,omitempty"`
}

// Listen creates a unix socket used for control the daemon
func (d *Daemon) Listen() (err error) {
	l, err := net.Listen("unix", filepath.Join(d.supDir, "immortal.sock"))
	if err != nil {
		return
	}
	router := violetear.New()
	router.Verbose = false
	router.HandleFunc("/", d.HandleStatus)
	router.HandleFunc("/signal/*", d.HandleSignal)
	// close socket when process finishes (after cmd.Wait())
	srv := &http.Server{Handler: router}
	d.wg.Add(1)
	go func() {
		defer d.wg.Done()
		err := srv.Serve(l)
		log.Printf("removing [%s/immortal.sock] %v\n", d.supDir, err)
	}()
	go func(quit chan struct{}) {
		<-quit
		if err := srv.Close(); err != nil {
			log.Printf("HTTP socket close error: %v", err)
		}
	}(d.quit)
	return
}

// HandleStatus return process status
func (d *Daemon) HandleStatus(w http.ResponseWriter, r *http.Request) {
	d.RLock()
	defer d.RUnlock()
	status := Status{
		Cmd:   strings.Join(d.cfg.command, " "),
		Count: d.count,
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

	// return status in json
	w.Header().Set("Content-Type", "application/json")
	if err := json.NewEncoder(w).Encode(status); err != nil {
		log.Println(err)
	}
}
