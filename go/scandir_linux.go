// +build linux

package immortal

import (
	"fmt"
	"log"
	"os"
	"path/filepath"
	"strings"
	"time"

	"github.com/immortal/xtime"
)

// ScanDir struct
type ScanDir struct {
	scandir       string
	sdir          string
	services      map[string]string
	timeMultipler time.Duration
}

// NewScanDir returns ScanDir struct
func NewScanDir(path string) (*ScanDir, error) {
	if info, err := os.Stat(path); err != nil {
		return nil, fmt.Errorf("%q no such file or directory", path)
	} else if !info.IsDir() {
		return nil, fmt.Errorf("%q is not a directory", path)
	}

	dir, err := filepath.EvalSymlinks(path)
	if err != nil {
		return nil, err
	}

	dir, err = filepath.Abs(dir)
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

	return &ScanDir{
		scandir:       dir,
		sdir:          GetSdir(),
		services:      map[string]string{},
		timeMultipler: 5,
	}, nil
}

// Start scans directory every 5 seconds
func (s *ScanDir) Start(ctl Control) {
	log.Printf("immortal scandir: %s", s.scandir)
	s.Scanner(ctl)
	ticker := time.NewTicker(time.Second * s.timeMultipler)
	for {
		select {
		case <-ticker.C:
			s.Scanner(ctl)
		}
	}
}

// Scanner searches for run.yml if file changes it will reload(stop-start)
func (s *ScanDir) Scanner(ctl Control) {
	// var services used to keep track of what services should be removed if they don't
	// exist any more
	var services []string

	find := func(path string, f os.FileInfo, err error) error {
		var (
			stop, start bool
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
				return fmt.Errorf("error getting the md5sum: %s", err)
			}
			// add service to services map or reload if file has been changed
			services = append(services, name)
			if hash, ok := s.services[name]; !ok || !isFile(filepath.Join(s.sdir, name, "lock")) {
				start = true
			} else if hash != md5 {
				stop = true
			}
			s.services[name] = md5
			// check if file hasn't been changed since last tick (5 seconds)
			refresh := (time.Now().Unix() - xtime.Get(f).Ctime().Unix()) <= int64(s.timeMultipler)
			if refresh || start {
				if stop {
					// restart = term + start
					log.Printf("Restarting: %s\n", name)
					ctl.SendSignal(filepath.Join(s.sdir, name, "immortal.sock"), "halt")
				}
				log.Printf("Starting: %s\n", name)
				// try to start before via socket
				if _, err := ctl.SendSignal(filepath.Join(s.sdir, name, "immortal.sock"), "start"); err != nil {
					if out, err := ctl.Run(fmt.Sprintf("immortal -c %s -ctl %s", path, name)); err != nil {
						// keep retrying
						delete(s.services, name)
						log.Println(err)
					} else {
						log.Printf("%s\n", out)
					}
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

	// halts services that don't exist anymore
	for service := range s.services {
		if !inSlice(services, service) {
			delete(s.services, service)
			ctl.SendSignal(filepath.Join(s.sdir, service, "immortal.sock"), "halt")
			log.Printf("Exiting: %s\n", service)
		}
	}
}
