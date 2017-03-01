package immortal

import (
	"fmt"
	"log"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"time"
)

// ScanDir struct
type ScanDir struct {
	scandir string
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

	return &ScanDir{
		scandir: dir,
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
func (s *ScanDir) Scaner() {
	time := time.Now()
	find := func(path string, f os.FileInfo, err error) error {
		if err != nil {
			return err
		}
		if strings.HasSuffix(f.Name(), ".yml") {
			name := strings.TrimSuffix(f.Name(), filepath.Ext(f.Name()))
			refresh := (time.Unix() - f.ModTime().Unix()) <= 5
			log.Printf("name: %s  refresh: %v", name, refresh)
			if refresh {
				cmd := exec.Command("immortal", "-c", path, "-ctl", name)
				stdoutStderr, err := cmd.CombinedOutput()
				if err != nil {
					return err
				}
				log.Printf("%s\n", stdoutStderr)
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
