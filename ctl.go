package immortal

import (
	"fmt"
	"io/ioutil"
	"os"
	"path/filepath"
)

type ServiceStatus struct {
	Name   string
	Socket string
	Status *Status
}

// GetStatus returns service status in json format
func GetStatus(socket string) (*Status, error) {
	status := &Status{}
	if err := GetJSON(socket, "/", status); err != nil {
		return nil, err
	}
	return status, nil
}

// FindServices return [name, socket path] of service
func FindServices(dir string) ([]*ServiceStatus, error) {
	sdir, err := ioutil.ReadDir(dir)
	if err != nil {
		return nil, err
	}
	var sockets []*ServiceStatus
	for _, file := range sdir {
		if file.IsDir() {
			socket := filepath.Join(dir, file.Name(), "immortal.sock")
			if fi, err := os.Lstat(socket); err == nil {
				if fi.Mode()&os.ModeType == os.ModeSocket {
					sockets = append(sockets,
						&ServiceStatus{
							file.Name(),
							socket,
							&Status{},
						},
					)
				}
			}
		}
	}
	return sockets, nil
}

// PurgeServices remove unused service directory
func PurgeServices(dir string) error {
	fmt.Printf("dir = %+v\n", dir)
	return nil
	//	return os.RemoveAll(filepath.Dir(dir))
}
