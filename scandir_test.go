// +build freebsd netbsd openbsd dragonfly darwin

package immortal

import (
	"fmt"
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
	mc.j++
	expect(mc.t, mc.expect[mc.i].socket, socket)
	expect(mc.t, mc.expect[mc.i].signal[mc.j], signal)
	if mc.expect[mc.i].signalErr {
		return nil, fmt.Errorf("error")
	}
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
	cmd := fmt.Sprintf("immortal -c %s/run.yml -ctl run", mc.expect[mc.i].cmd)
	expect(mc.t, cmd, command)
	if mc.expect[mc.i].runErr != "" {
		return nil, fmt.Errorf("%s\n", mc.expect[mc.i].runErr)
	}
	return []byte(fmt.Sprintf("started %d", mc.i)), nil
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
		t: t,
		i: 0,  //  number of scan calls
		j: -1, // number of calls within the same Scan
		expect: []struct {
			socket      string
			signal      []string
			signalErr   bool
			cmd, runErr string
		}{
			{"/var/run/immortal/run/immortal.sock", []string{}, true, s.scandir, ""},
			{"/var/run/immortal/run/immortal.sock", []string{"halt", "start"}, true, s.scandir, "return error 1"},
			{"/var/run/immortal/run/immortal.sock", []string{"halt"}, false, "", ""},
			{"/var/run/immortal/run/immortal.sock", []string{"start"}, true, s.scandir, "can't start"},
			{"/var/run/immortal/run/immortal.sock", []string{"start"}, false, "", ""},
			{"/var/run/immortal/run/immortal.sock", []string{}, false, "", ""},
			{"/var/run/immortal/run/immortal.sock", []string{}, false, "", ""},
			{"/var/run/immortal/run/immortal.sock", []string{"start"}, false, "", ""},
			{"/var/run/immortal/run/immortal.sock", []string{"halt"}, false, "", ""},
		},
		status: make(chan string, 1),
	}
	// start scanner loop
	go s.Start(ctl)
	if err = ioutil.WriteFile(filepath.Join(dir, "run.yml"), []byte("stage 0"), 0644); err != nil {
		t.Fatal(err)
	}
	var status string
	status = <-ctl.status
	expect(t, status, "Run")
	expect(t, "2bf41d668dd3b0909d58f982aff35a25", s.services["run"])
	ctl.i++
	ctl.j = -1

	// change service contents, a restart (exit, start) is expected
	if err = ioutil.WriteFile(filepath.Join(dir, "run.yml"), []byte("stage 1"), 0644); err != nil {
		t.Fatal(err)
	}
	status = <-ctl.status
	expect(t, status, "halt")
	status = <-ctl.status
	expect(t, status, "start")
	ctl.i++
	ctl.j = -1
}
