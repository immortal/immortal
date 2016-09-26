package immortal

import (
	"encoding/json"
	"fmt"
	"io/ioutil"
	"log"
	"net"
	"net/http"
	"os"
	"path/filepath"
	"strconv"
	"strings"
	"syscall"
	"time"

	"github.com/nbari/violetear"
)

// Supervisor interface
type Supervisor interface {
	HandleSignals(signal string, d *Daemon)
	Info(ch <-chan os.Signal, d *Daemon)
	IsRunning(pid int) bool
	ReadPidFile(pidfile string) (int, error)
	WatchPid(pid int, ch chan<- error)
}

// Sup implements Supervisor
type Sup struct {
	daemon  *Daemon
	process *process
}

// IsRunning check if process is running
func (s *Sup) IsRunning(pid int) bool {
	process, _ := os.FindProcess(pid)
	if err := process.Signal(syscall.Signal(0)); err != nil {
		return false
	}
	return true
}

// ReadPidFile read pid from file if error returns pid 0
func (s *Sup) ReadPidFile(pidfile string) (int, error) {
	content, err := ioutil.ReadFile(pidfile)
	if err != nil {
		return 0, err
	}
	lines := strings.Split(string(content), "\n")
	pid, err := strconv.Atoi(lines[0])
	if err != nil {
		return 0, err
	}
	return pid, nil
}

// ReadSocket read from socket and handled by signals
// curl --unix-socket immortal.sock http:/
func (s *Sup) ReadSocket(supDir string) {
	//s.HandleSignals(fifo.msg, d)
	l, err := net.Listen("unix", filepath.Join(supDir, "immortal.sock"))
	if err != nil {
		log.Println(err)
	}
	router := violetear.New()
	router.HandleFunc("/", s.Status)
	err = http.Serve(l, router)
	if err != nil {
		log.Println(err)
	}
}

// Status return process status
func (s *Sup) Status(w http.ResponseWriter, r *http.Request) {
	j := map[string]string{
		"uptime": fmt.Sprintf("%s", time.Since(s.process.sTime)),
		"PID":    fmt.Sprintf("%d", s.process.Pid()),
		"dir":    s.daemon.supDir,
	}
	if err := json.NewEncoder(w).Encode(j); err != nil {
		log.Println(err)
	}
}
