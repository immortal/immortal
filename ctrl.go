package immortal

import (
	"os"
	"path/filepath"
)

func CreateSuperviseDir(f FIFOer, path string) (err error) {
	wd := filepath.Join(path, "supervise")
	if err = os.MkdirAll(wd, 0700); err != nil {
		return
	}
	// create control pipe
	err = f.Make(filepath.Join(wd, "control"))
	if err != nil {
		return
	}
	// create status pipe
	err = f.Make(filepath.Join(wd, "ok"))
	if err != nil {
		return
	}
	// create lock
	if err = Lock(filepath.Join(wd, "lock")); err != nil {
		return
	}
	return
}
