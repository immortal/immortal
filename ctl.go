package immortal

import (
	"io/ioutil"
	"os"
	"path/filepath"
)

// GetStatus returns service status in json format
func GetStatus(socket string) (*Status, error) {
	status := &Status{}
	if err := GetJSON(socket, "/", status); err != nil {
		return nil, err
	}
	return status, nil
}

// FindServices return [name, socket path] of service
func FindServices(dir string) ([][]string, error) {
	sdir, err := ioutil.ReadDir(dir)
	if err != nil {
		return nil, err
	}
	sockets := [][]string{}
	for _, file := range sdir {
		if file.IsDir() {
			socket := filepath.Join(dir, file.Name(), "immortal.sock")
			if fi, err := os.Lstat(socket); err == nil {
				if fi.Mode()&os.ModeType == os.ModeSocket {
					sockets = append(sockets, []string{file.Name(), socket})
				}
			}
		}
	}
	return sockets, nil
}

// PurgeServices remove unused service directory
func PurgeServices(dir string) error {
	return os.RemoveAll(filepath.Dir(dir))
}
