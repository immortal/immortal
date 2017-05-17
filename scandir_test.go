// +build freebsd netbsd openbsd dragonfly darwin

package immortal

import (
	"io/ioutil"
	"log"
	"os"
	"path/filepath"
	"testing"
)

func TestNewScanDir(t *testing.T) {
	_, err := NewScanDir("/tmp")
	if err != nil {
		t.Error(err)
	}
}

func TestNewScanDirNonexistent(t *testing.T) {
	_, err := NewScanDir("/dev/null/non-existent")
	if err == nil {
		t.Error("Expecting error")
	}
}

type mockController struct {
	t      *testing.T
	i, j   int
	expect []struct {
		socket      string
		signal      []string
		signalErr   bool
		cmd, runErr string
	}
	status chan string
}

func (mc *mockController) GetStatus(socket string) (*Status, error) {
	status := &Status{}
	return status, nil
}

func (mc *mockController) SendSignal(socket, signal string) (*SignalResponse, error) {
	defer func() {
		mc.status <- signal
	}()
	return nil, nil
}

func (mc *mockController) FindServices(dir string) ([]*ServiceStatus, error) {
	return nil, nil
}

func (mc *mockController) PurgeServices(dir string) error {
	return nil
}

func (mc *mockController) Run(command string) ([]byte, error) {
	defer func() {
		mc.status <- "Run"
	}()
	return nil, nil
}

func TestScanner(t *testing.T) {
	dir, err := ioutil.TempDir("", "scaner")
	if err != nil {
		log.Fatal(err)
	}
	defer os.RemoveAll(dir) // clean up
	s, err := NewScanDir(dir)
	if err != nil {
		t.Fatal(err)
	}
	ctl := &mockController{
		status: make(chan string),
	}
	if err = ioutil.WriteFile(filepath.Join(dir, "run.yml"), []byte("stage 0"), 0644); err != nil {
		t.Fatal(err)
	}

	// start scanner loop
	go s.Start(ctl)

	var status string
	status = <-ctl.status
	expect(t, status, "Run")
	expect(t, "2bf41d668dd3b0909d58f982aff35a25", s.services["run"])

	// change service contents, a restart (exit, start) is expected
	if err = ioutil.WriteFile(filepath.Join(dir, "run.yml"), []byte("stage 1"), 0644); err != nil {
		t.Fatal(err)
	}
	status = <-ctl.status
	expect(t, status, "halt")
	status = <-ctl.status
	expect(t, status, "start")

	// remove service, exit
	if err := os.Remove(filepath.Join(dir, "run.yml")); err != nil {
		t.Fatal(err)
	}

}
