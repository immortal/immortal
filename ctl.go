package immortal

import (
	"fmt"
	"io/ioutil"
	"os"
	"path/filepath"
)

type Control interface {
	GetStatus(socket string) (*Status, error)
	SendSignal(socket, signal string) (*SignalResponse, error)
	FindServices(dir string) ([]*ServiceStatus, error)
	PurgeServices(dir string) error
}

// ServiceStatus struct
type ServiceStatus struct {
	Name           string
	Socket         string
	Status         *Status
	SignalResponse *SignalResponse
}

type Controller struct{}

// GetStatus returns service status in json format
func (c *Controller) GetStatus(socket string) (*Status, error) {
	status := &Status{}
	if err := GetJSON(socket, "/", status); err != nil {
		return nil, err
	}
	return status, nil
}

// SendSignal send signal to process
func (c *Controller) SendSignal(socket, signal string) (*SignalResponse, error) {
	res := &SignalResponse{}
	if err := GetJSON(socket, fmt.Sprintf("/signal/%s", signal), res); err != nil {
		return nil, err
	}
	return res, nil
}

// FindServices return [name, socket path] of service
func (c *Controller) FindServices(dir string) ([]*ServiceStatus, error) {
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
							&SignalResponse{},
						},
					)
				}
			}
		}
	}
	return sockets, nil
}

// PurgeServices remove unused service directory
func (c *Controller) PurgeServices(dir string) error {
	return os.RemoveAll(filepath.Dir(dir))
}
