package immortal

import (
	"io/ioutil"
	"net"
	"os"
	"path/filepath"
	"testing"
)

func TestGetStatus(t *testing.T) {
	if _, err := GetStatus("/dev/null"); err == nil {
		t.Errorf("Expecting an error")
	}
}

func TestSendSignal(t *testing.T) {
	if _, err := SendSignal("/dev/null", "test"); err == nil {
		t.Errorf("Expecting an error")
	}
}

func TestFindServices(t *testing.T) {
	dir, err := ioutil.TempDir("", "FindServices")
	if err != nil {
		t.Error(err)
	}
	defer os.RemoveAll(dir)
	tdir := filepath.Join(dir, "test")
	os.Mkdir(tdir, 0700)
	_, err = net.Listen("unix", filepath.Join(tdir, "immortal.sock"))
	if err != nil {
		t.Error(err)
	}
	s, err := FindServices(dir)
	if err != nil {
		t.Error(err)
	}
	expect(t, 1, len(s))
}

func TestFindServicesNonexistent(t *testing.T) {
	_, err := FindServices("/dev/null/non-existent")
	if err == nil {
		t.Errorf("Expecting an error")
	}
}

func TestPurgeServices(t *testing.T) {
	dir, err := ioutil.TempDir("", "PurgeServices")
	if err != nil {
		t.Error(err)
	}
	defer os.RemoveAll(dir)
	tdir := filepath.Join(dir, "test")
	os.Mkdir(tdir, 0700)
	os.OpenFile(filepath.Join(tdir, "f1"), os.O_RDONLY|os.O_CREATE, 0640)
	os.OpenFile(filepath.Join(tdir, "f2"), os.O_RDONLY|os.O_CREATE, 0640)
	os.OpenFile(filepath.Join(tdir, "f3"), os.O_RDONLY|os.O_CREATE, 0640)
	files, _ := ioutil.ReadDir(tdir)
	expect(t, 3, len(files))
	err = PurgeServices(tdir)
	if err != nil {
		t.Error(err)
	}
	files, _ = ioutil.ReadDir(tdir)
	expect(t, 0, len(files))
}
