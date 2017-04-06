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
	"time"
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
}

func (mc *mockController) GetStatus(socket string) (*Status, error) {
	status := &Status{}
	return status, nil
}

func (mc *mockController) SendSignal(socket, signal string) (*SignalResponse, error) {
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
	cmd := fmt.Sprintf("immortal -c %s/run.yml -ctl run", mc.expect[mc.i].cmd)
	expect(mc.t, cmd, command)
	if mc.expect[mc.i].runErr != "" {
		return nil, fmt.Errorf("%s\n", mc.expect[mc.i].runErr)
	}
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
	s, err := NewScanDir(dir)
	if err != nil {
		t.Fatal(err)
	}
	s.timeMultipler = 1
	if err = ioutil.WriteFile(filepath.Join(dir, "run.yml"), []byte("stage 0"), 0644); err != nil {
		t.Fatal(err)
	}
	ctl := &mockController{
		t: t,
		i: 0,  //  number of scan calls
		j: -1, // number of calls within the same  Scan
		expect: []struct {
			socket      string
			signal      []string
			signalErr   bool
			cmd, runErr string
		}{
			{"/var/run/immortal/run/immortal.sock", []string{"start"}, true, s.scandir, ""},
			{"/var/run/immortal/run/immortal.sock", []string{"halt", "start"}, true, s.scandir, "return error 1"},
			{"/var/run/immortal/run/immortal.sock", []string{"exit"}, false, "", ""},
			{"/var/run/immortal/run/immortal.sock", []string{"start"}, true, s.scandir, "can't start"},
			{"/var/run/immortal/run/immortal.sock", []string{"start"}, false, "", ""},
			{"/var/run/immortal/run/immortal.sock", []string{}, false, "", ""},
			{"/var/run/immortal/run/immortal.sock", []string{}, false, "", ""},
			{"/var/run/immortal/run/immortal.sock", []string{"start"}, false, "", ""},
			{"/var/run/immortal/run/immortal.sock", []string{"exit"}, false, "", ""},
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
	if err = ioutil.WriteFile(filepath.Join(dir, "run.yml"), []byte("stage 1"), 0644); err != nil {
		t.Fatal(err)
	}
	s.Scaner(ctl)
	// if error while starting, the service will be removed in order to keep retrying
	expect(t, len(s.services), 0)
	re = regexp.MustCompile(`return error 1`)
	expect(t, "return error 1", re.FindString(buf.String()))
	buf.Reset()
	ctl.i++
	ctl.j = -1

	// remove service, exit
	if err := os.Remove(filepath.Join(dir, "run.yml")); err != nil {
		t.Fatal(err)
	}
	s.Scaner(ctl)
	ctl.i++
	ctl.j = -1
	expect(t, 0, len(s.services))

	// new service
	if err = ioutil.WriteFile(filepath.Join(dir, "run.yml"), []byte("stage 2"), 0644); err != nil {
		t.Fatal(err)
	}
	s.Scaner(ctl)
	expect(t, len(s.services), 0)
	re = regexp.MustCompile(`Starting: run`)
	expect(t, "Starting: run", re.FindString(buf.String()))
	re = regexp.MustCompile(`can't start`)
	expect(t, "can't start", re.FindString(buf.String()))
	buf.Reset()
	ctl.i++
	ctl.j = -1

	// scan again and send signal START because it has passed less than 5 sec
	s.Scaner(ctl)
	expect(t, "9944429f23907af240460d0583a27cd2", s.services["run"])
	expect(t, 1, len(s.services))
	buf.Reset()
	ctl.i++
	ctl.j = -1

	// NO refresh
	time.Sleep(time.Second * 2)
	s.Scaner(ctl)
	expect(t, "9944429f23907af240460d0583a27cd2", s.services["run"])
	expect(t, 1, len(s.services))
	buf.Reset()
	ctl.i++
	ctl.j = -1

	// NO refresh 2
	s.Scaner(ctl)
	expect(t, "9944429f23907af240460d0583a27cd2", s.services["run"])
	expect(t, 1, len(s.services))
	buf.Reset()
	ctl.i++
	ctl.j = -1

	// Touch expect a start
	mtime := time.Now().UTC()
	atime := time.Now().UTC()
	if err := os.Chtimes(filepath.Join(dir, "run.yml"), atime, mtime); err != nil {
		log.Fatal(err)
	}
	s.Scaner(ctl)
	expect(t, "9944429f23907af240460d0583a27cd2", s.services["run"])
	expect(t, 1, len(s.services))
	buf.Reset()
	ctl.i++
	ctl.j = -1

	//permission log
	s.scandir = "/dev/null/non-existent"
	s.Scaner(ctl)
	expect(t, 0, len(s.services))
}
