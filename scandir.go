package immortal

import (
	"fmt"
	"log"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"time"

	"github.com/immortal/xtime"
)

// ScanDir struct
type ScanDir struct {
	scandir  string
	sdir     string
	services map[string]string
}

// NewScanDir returns ScanDir struct
func NewScanDir(path string) (*ScanDir, error) {
	if info, err := os.Stat(path); err != nil {
		return nil, fmt.Errorf("%q no such file or directory.", path)
	} else if !info.IsDir() {
		return nil, fmt.Errorf("%q is not a directory.", path)
	}

	dir, err := filepath.EvalSymlinks(path)
	if err != nil {
		return nil, err
	}

	dir, err = filepath.Abs(filepath.Clean(dir))
	if err != nil {
		return nil, err
	}

	d, err := os.Open(dir)
	if err != nil {
		if os.IsPermission(err) {
			return nil, os.ErrPermission
		}
		return nil, err
	}
	defer d.Close()

	// if IMMORTAL_SDIR env is set, use it as default sdir
	sdir := os.Getenv("IMMORTAL_SDIR")
	if sdir == "" {
		sdir = "/var/run/immortal"
	}

	return &ScanDir{
		scandir:  dir,
		sdir:     sdir,
		services: map[string]string{},
	}, nil
}

// Start scans directory every 5 seconds
func (s *ScanDir) Start() {
	log.Printf("immortal scandir: %s", s.scandir)
	s.Scaner()
	ticker := time.NewTicker(5 * time.Second)
	for {
		select {
		case <-ticker.C:
			s.Scaner()
		}
	}
}

// Scaner searches for run.yml if file changes it will reload(exit-start)
func (s *ScanDir) Scaner() {
	// var services used to keep track of what services should be removed if they don't
	// exist any more
	var services []string

	find := func(path string, f os.FileInfo, err error) error {
		var (
			exit, start bool
			md5, name   string
		)
		if err != nil {
			return err
		}
		// only use .yml files
		if strings.HasSuffix(f.Name(), ".yml") {
			name = strings.TrimSuffix(f.Name(), filepath.Ext(f.Name()))
			md5, err = md5sum(path)
			if err != nil {
				return err
			}
			// add service to services map or reload if file has been changed
			services = append(services, name)
			if hash, ok := s.services[name]; !ok {
				s.services[name] = md5
				start = true
			} else if hash != md5 {
				exit = true
			}
			// check if file hasn't been changed since last tick (5 seconds)
			refresh := (time.Now().Unix() - xtime.Get(f).Ctime().Unix()) <= 5
			if refresh || start {
				if exit {
					// restart = exit + start
					log.Printf("Restarting: %s\n", name)
					SendSignal(filepath.Join(s.sdir, name, "immortal.sock"), "exit")
					time.Sleep(time.Second)
				}
				log.Printf("Starting: %s\n", name)
				// try to start before via socket
				if _, err := SendSignal(filepath.Join(s.sdir, name, "immortal.sock"), "start"); err != nil {
					cmd := exec.Command("immortal", "-c", path, "-ctl", name)
					cmd.Env = os.Environ()
					stdoutStderr, err := cmd.CombinedOutput()
					if err != nil {
						return err
					}
					log.Printf("%s\n", stdoutStderr)
				}
			}
		}
		return nil
	}

	// find for .yml files
	err := filepath.Walk(s.scandir, find)
	if err != nil && !os.IsPermission(err) {
		log.Println(err)
	}

	// exit services that don't exist anymore
	for service := range s.services {
		if !inSlice(services, service) {
			delete(s.services, service)
			SendSignal(filepath.Join(s.sdir, service, "immortal.sock"), "exit")
			log.Printf("Exiting: %s\n", service)
		}
	}
}
