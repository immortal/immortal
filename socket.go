package immortal

import (
	"encoding/json"
	"fmt"
	"log"
	"net"
	"net/http"
	"os"
	"path/filepath"
	"time"

	"github.com/nbari/violetear"
)

// Listen creates a unix socket used for control the daemon
func (d *Daemon) Listen() error {
	l, err := net.Listen("unix", filepath.Join(d.supDir, "immortal.sock"))
	if err != nil {
		return err
	}
	router := violetear.New()
	router.HandleFunc("/", d.Status)
	go http.Serve(l, router)
	return nil
}

// Status return process status
func (d *Daemon) Status(w http.ResponseWriter, r *http.Request) {
	j := map[string]string{
		"uptime": fmt.Sprintf("%s", time.Since(d.sTime)),
		"PID":    fmt.Sprintf("%d", os.Getpid()),
		"dir":    d.supDir,
	}
	if err := json.NewEncoder(w).Encode(j); err != nil {
		log.Println(err)
	}
}
