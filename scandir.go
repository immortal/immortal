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
	services map[string]string
	watch    chan string
	sync.Mutex
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
		scandir:  dir,
		sdir:     GetSdir(),
		services: map[string]string{},
		watch:    make(chan string, 1),
	}, nil
}

// Start check for changes on directory
func (s *ScanDir) Start(ctl Control) {
	log.Printf("immortal scandir: %s", s.scandir)

	// check for new services on scandir
	go WatchDir(s.scandir, s.watch)
	s.watch <- s.scandir

	for {
		select {
		case watch := <-s.watch:
			switch watch {
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
					s.Lock()
					// restart if file changed
					if md5 != s.services[serviceName] {
						s.services[serviceName] = md5
						log.Printf("Stopping: %s\n", serviceName)
						ctl.SendSignal(filepath.Join(s.sdir, serviceName, "immortal.sock"), "halt")
					}
					s.Unlock()
					log.Printf("Starting: %s\n", serviceName)
					// try to start before via socket
					if _, err := ctl.SendSignal(filepath.Join(s.sdir, serviceName, "immortal.sock"), "start"); err != nil {
						if out, err := ctl.Run(fmt.Sprintf("immortal -c %s -ctl %s", watch, serviceName)); err != nil {
							// keep retrying
							delete(s.services, serviceName)
							log.Println(err)
						} else {
							log.Printf("%s\n", out)
						}
					}
					go func() {
						if err := WatchFile(watch, s.watch); err != nil {
							log.Printf("WatchFile error: %s", err)
							// try 3 times sleeping i*100ms between retries
							for i := int32(100); i <= 300; i += 100 {
								time.Sleep(time.Duration(rand.Int31n(i)) * time.Millisecond)
								err := WatchFile(watch, s.watch)
								if err == nil {
									return
								}
							}
							log.Printf("Could not watch file %q error: %s", watch, err)
						}
					}()
				} else {
					// remove service
					s.Lock()
					delete(s.services, serviceName)
					s.Unlock()
					ctl.SendSignal(filepath.Join(s.sdir, serviceName, "immortal.sock"), "halt")
					log.Printf("Exiting: %s\n", serviceName)
				}
			}
		}
	}
}

// Scaner searches for *.yml if file changes it will reload(stop-start)
func (s *ScanDir) Scandir(ctl Control) error {
	s.Lock()
	defer s.Unlock()
	find := func(path string, f os.FileInfo, err error) error {
		if err != nil {
			return err
		}
		if f.Mode().IsRegular() {
			if filepath.Ext(f.Name()) == ".yml" {
				name := strings.TrimSuffix(f.Name(), filepath.Ext(f.Name()))
				md5, err := md5sum(path)
				if err != nil {
					return fmt.Errorf("Error getting the md5sum: %s", err)
				}
				if _, ok := s.services[name]; !ok {
					s.services[name] = md5
					log.Printf("Starting: %s\n", name)
					if out, err := ctl.Run(fmt.Sprintf("immortal -c %s -ctl %s", path, name)); err != nil {
						log.Println(err)
					} else {
						log.Printf("%s\n", out)
					}
					go func() {
						if err := WatchFile(path, s.watch); err != nil {
							log.Printf("WatchFile error: %s", err)
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
					}()
				}
			}
		}

		// Block for 100 ms on each call to kevent (WatchFile)
		time.Sleep(100 * time.Millisecond)

		return err
	}
	return filepath.Walk(s.scandir, find)
}
