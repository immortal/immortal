// +build freebsd netbsd openbsd dragonfly darwin

package immortal

import (
	"fmt"
	"log"
	"math/rand"
	"os"
	"path/filepath"
	"strings"
	"sync"
	"time"
)

// ScanDir struct
type ScanDir struct {
	scandir  string
	sdir     string
	services sync.Map
	watch    chan string
}

// NewScanDir returns ScanDir struct
func NewScanDir(path string) (*ScanDir, error) {
	if !isDir(path) {
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
		scandir: dir,
		sdir:    GetSdir(),
		watch:   make(chan string, 1),
	}, nil
}

// Start check for changes on directory
func (s *ScanDir) Start(ctl Control) {
	log.Printf("immortal scandir: %s", s.scandir)

	// create supervise directory (/var/run/immortal) if doesn't exists
	// IMMORTAL_SDIR
	if !isDir(s.sdir) {
		if err := os.MkdirAll(s.sdir, os.ModePerm); err != nil {
			log.Fatalf("Could not create supervise dir: %q, %v", s.sdir, err)
		}
	}

	// check for changes on sdir helps to restart stoped services
	go WatchDir(s.sdir, s.watch)

	// check for new services on scandir
	go WatchDir(s.scandir, s.watch)

	// start with scandir
	s.watch <- s.scandir

	ticker := time.NewTicker(time.Second * 5)

	for {
		select {
		// every 5 seconds
		case <-ticker.C:
			s.watch <- s.scandir
		// based on kqueue response
		case watch := <-s.watch:
			switch watch {
			case s.sdir:
				// RESTART, after halting a service, this will
				// start stopped services after 1 second of receiving the signal
				time.Sleep(time.Second)
				if err := s.Scandir(ctl); err != nil && !os.IsPermission(err) {
					log.Printf("Scandir error: %s", err)
				}
			case s.scandir:
				if err := s.Scandir(ctl); err != nil && !os.IsPermission(err) {
					log.Printf("Scandir error: %s", err)
				}
			default:
				serviceFile := filepath.Base(watch)
				serviceName := strings.TrimSuffix(serviceFile, filepath.Ext(serviceFile))
				if isFile(watch) {
					md5, err := md5sum(watch)
					if err != nil {
						log.Fatalf("Error getting the md5sum: %s", err)
					}
					// restart if file changed
					hash, _ := s.services.Load(serviceName)
					if md5 != hash {
						s.services.Store(serviceName, md5)
						log.Printf("Stopping: %s\n", serviceName)
						ctl.SendSignal(filepath.Join(s.sdir, serviceName, "immortal.sock"), "halt")
					}
					log.Printf("Starting: %s\n", serviceName)
					// try to start before via socket
					if _, err := ctl.SendSignal(filepath.Join(s.sdir, serviceName, "immortal.sock"), "start"); err != nil {
						if out, err := ctl.Run(fmt.Sprintf("immortal -c %s -ctl %s", watch, serviceName)); err != nil {
							// keep retrying
							s.services.Delete(serviceName)
							log.Println(err)
						} else {
							log.Printf("%s\n", out)
						}
					}
					go s.WatchFile(watch)
				} else {
					// remove service
					s.services.Delete(serviceName)
					ctl.SendSignal(filepath.Join(s.sdir, serviceName, "immortal.sock"), "halt")
					log.Printf("Exiting: %s\n", serviceName)
				}
			}
		}
	}
}

// Scandir searches for *.yml if file changes it will reload(stop-start)
func (s *ScanDir) Scandir(ctl Control) error {
	find := func(path string, f os.FileInfo, err error) error {
		if err != nil {
			return err
		}
		if f.Mode().IsRegular() {
			if filepath.Ext(f.Name()) == ".yml" {
				name := strings.TrimSuffix(f.Name(), filepath.Ext(f.Name()))
				md5, err := md5sum(path)
				if err != nil {
					return fmt.Errorf("error getting the md5sum: %s", err)
				}
				// start or restart if service is not in map or file lock don't exists
				if _, ok := s.services.Load(name); !ok || !isFile(filepath.Join(s.sdir, name, "lock")) {
					s.services.Store(name, md5)
					log.Printf("Starting: %s\n", name)
					if out, err := ctl.Run(fmt.Sprintf("immortal -c %s -ctl %s", path, name)); err != nil {
						log.Println(err)
					} else {
						log.Printf("%s\n", out)
					}
					go s.WatchFile(path)
				}
			}
		}

		// Block for 100 ms on each call to kevent (WatchFile)
		time.Sleep(100 * time.Millisecond)

		return err
	}
	return filepath.Walk(s.scandir, find)
}

// WatchFile - react on file changes
func (s *ScanDir) WatchFile(path string) {
	if err := WatchFile(path, s.watch); err != nil {
		// try 3 times sleeping i*100ms between retries
		for i := int32(100); i <= 300; i += 100 {
			time.Sleep(time.Duration(rand.Int31n(i)) * time.Millisecond)
			err := WatchFile(path, s.watch)
			if err == nil {
				return
			}
		}
		log.Printf("Could not watch file %q error: %s", path, err)
	}
}
