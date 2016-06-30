package immortal

import (
	"fmt"
	"log"
	"os"
	"path/filepath"
	"time"
)

type ScanDir struct {
	scandir string
}

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

func (self *ScanDir) Start() {
	log.Println(self.scandir)
	ticker := time.NewTicker(5 * time.Second)
	for {
		select {
		case <-ticker.C:
			self.Scaner()
		}
	}
}

func (self *ScanDir) Scaner() {
	t := time.Now().UTC().Format(time.RFC3339)
	log.Printf("time: %q\n", t)
	find := func(path string, f os.FileInfo, err error) error {
		if err != nil {
			return err
		}

		var is_exec bool
		if m := f.Mode(); !m.IsDir() && m&0111 != 0 {
			is_exec = true
		}

		log.Printf("path: %s name: %s mode: [%v  %#o], is_exec: %v", path, f.Name(), f.Mode(), f.Mode(), is_exec)
		return nil
	}
	err := filepath.Walk(self.scandir, find)
	if err != nil {
		log.Println(err)
	}
	return
}
