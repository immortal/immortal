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
	if info, err := os.Stat(path); os.IsNotExist(err) {
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

// Scaner searchs for run.yml and based on the perms start/stops the process
// if file changes it will reload(exit-start)
func (s *ScanDir) Scaner() {
	find := func(path string, f os.FileInfo, err error) error {
		var (
			md5  string
			name string
			exit bool
		)
		if err != nil {
			return err
		}
		if strings.HasSuffix(f.Name(), ".yml") {
			name = strings.TrimSuffix(f.Name(), filepath.Ext(f.Name()))
			md5, err = md5sum(path)
			if err != nil {
				return err
			}
			// add service to services map or reload if file has been changed
			if hash, ok := s.services[name]; !ok {
				s.services[name] = md5
			} else if hash != md5 {
				exit = true
			}
			// check if file hasn't been changed since last tick (5 seconds)
			refresh := (time.Now().Unix() - xtime.Get(f).Ctime().Unix()) <= 5
			if refresh {
				// if file is executable start
				if m := f.Mode(); m&0111 != 0 {
					if exit {
						log.Printf("Restarting %q\n", name)
						SendSignal(filepath.Join(s.sdir, name, "immortal.sock"), "exit")
					}
					// try to start before via socket
					if _, err := SendSignal(filepath.Join(s.sdir, name, "immortal.sock"), "start"); err != nil {
						log.Printf("Starting %q\n", name)
						cmd := exec.Command("immortal", "-c", path, "-ctl", name)
						cmd.Env = os.Environ()
						fmt.Printf("cmd.Path = %+v\n", cmd.Path)
						stdoutStderr, err := cmd.CombinedOutput()
						if err != nil {
							return err
						}
						log.Printf("%s\n", stdoutStderr)
					} else {
						log.Printf("%q restarted\n", name)
					}
				} else {
					_, err := SendSignal(filepath.Join(s.sdir, name, "immortal.sock"), "down")
					if err != nil {
						log.Printf("%q not running\n", name)
					}
				}
			}
		}
		return nil
	}
	err := filepath.Walk(s.scandir, find)
	if err != nil {
		log.Println(err)
	}
	return
}
