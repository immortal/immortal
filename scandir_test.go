package immortal

import (
	"bytes"
	"fmt"
	"io/ioutil"
	"log"
	"os"
	"path/filepath"
	"regexp"
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
		signal, err []string
		cmd         string
	}
}

func (mc *mockController) GetStatus(socket string) (*Status, error) {
	status := &Status{}
	return status, nil
}

func (mc *mockController) SendSignal(socket, signal string) (*SignalResponse, error) {
	mc.j++
	expect(mc.t, mc.expect[mc.i].socket, socket)
	expect(mc.t, mc.expect[mc.i].signal[mc.j], signal)
	if mc.expect[mc.i].err[mc.j] != "" {
		return nil, fmt.Errorf(mc.expect[mc.i].err[mc.j])
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
	cmd := fmt.Sprintf("immortal -c %s/run.yml -ctl run", mc.expect[mc.i].cmd)
	expect(mc.t, cmd, command)
	return []byte(fmt.Sprintf("started %d", mc.i)), nil
}

func TestScaner(t *testing.T) {
	var buf bytes.Buffer
	log.SetOutput(&buf)
	dir, err := ioutil.TempDir("", "scaner")
	if err != nil {
		log.Fatal(err)
	}
	defer os.RemoveAll(dir) // clean up
	if dir, err = filepath.EvalSymlinks(dir); err != nil {
		t.Fatal(err)
	}
	s, err := NewScanDir(dir)
	if err != nil {
		t.Fatal(err)
	}
	err = ioutil.WriteFile(filepath.Join(dir, "run.yml"), []byte("stage 0"), 0644)
	if err != nil {
		t.Fatal(err)
	}
	ctl := &mockController{
		t: t,
		i: 0,  //  number of scan calls
		j: -1, // number of calls within the same  Scan
		expect: []struct {
			socket      string
			signal, err []string
			cmd         string
		}{
			{"/var/run/immortal/run/immortal.sock", []string{"start"}, []string{"starting 0"}, dir},
			{"/var/run/immortal/run/immortal.sock", []string{"exit", "start"}, []string{"this one is ignored", "starting 1"}, dir},
		},
	}
	// first call to scanner, should start services and create hashes
	s.Scaner(ctl)
	expect(t, "2bf41d668dd3b0909d58f982aff35a25", s.services["run"])
	re := regexp.MustCompile(`started 0`)
	expect(t, "started 0", re.FindString(buf.String()))
	buf.Reset()
	ctl.i++
	ctl.j = -1

	// change service contents, a restart (exit, start) is expected
	err = ioutil.WriteFile(filepath.Join(dir, "run.yml"), []byte("stage 1"), 0644)
	s.Scaner(ctl)
	expect(t, "0af0f52bb73880b58d20ec86a9c5b1dc", s.services["run"])
	re = regexp.MustCompile(`started 1`)
	expect(t, "started 1", re.FindString(buf.String()))
	buf.Reset()
	ctl.i++
	ctl.j = -1
	fmt.Printf("s.services = %+v\n", s.services)
}
