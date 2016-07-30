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
	// create control & ok  pipe
	for _, v := range []string{"control", "ok"} {
		err = f.Make(filepath.Join(wd, v))
		if err != nil {
			return
		}
	}
	return
}
