package immortal

import (
	"fmt"
	"io/ioutil"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
)

// Control interface
type Control interface {
	GetStatus(socket string) (*Status, error)
	SendSignal(socket, signal string) (*SignalResponse, error)
	FindServices(dir string) ([]*ServiceStatus, error)
	PurgeServices(dir string) error
	Run(command string) ([]byte, error)
}

// ServiceStatus struct
type ServiceStatus struct {
	Name           string
	Socket         string
	Status         *Status
	SignalResponse *SignalResponse
}

// Controller implements Control
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

// Run executes a command and print combinedOutput
func (c *Controller) Run(command string) ([]byte, error) {
	parts := strings.Fields(command)
	cmd := parts[0]
	arg := parts[1:len(parts)]
	run := exec.Command(cmd, arg...)
	run.Env = os.Environ()
	stdoutStderr, err := run.CombinedOutput()
	if err != nil {
		return nil, err
	}
	return stdoutStderr, err
}
