package immortal

import (
	"io/ioutil"
	"net"
	"os"
	"path/filepath"
	"testing"
)

func TestGetStatus(t *testing.T) {
	ctl := &Controller{}
	if _, err := ctl.GetStatus("/dev/null"); err == nil {
		t.Errorf("Expecting an error")
	}
}

func TestSendSignal(t *testing.T) {
	ctl := &Controller{}
	if _, err := ctl.SendSignal("/dev/null", "test"); err == nil {
		t.Errorf("Expecting an error")
	}
}

func TestFindServices(t *testing.T) {
	ctl := &Controller{}
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
	s, err := ctl.FindServices(dir)
	if err != nil {
		t.Error(err)
	}
	expect(t, 1, len(s))
}

func TestFindServicesNonexistent(t *testing.T) {
	ctl := &Controller{}

	_, err := ctl.FindServices("/dev/null/non-existent")
	if err == nil {
		t.Errorf("Expecting an error")
	}
}

func TestPurgeServices(t *testing.T) {
	ctl := &Controller{}

	dir, err := ioutil.TempDir("", "PurgeServices")
	if err != nil {
		t.Error(err)
	}
	defer os.RemoveAll(dir)
	err = ctl.PurgeServices(dir)
	if err == nil {
		t.Error("Expecting an error")
	}
	tdir := filepath.Join(dir, "test")
	os.Mkdir(tdir, 0700)
	os.Create(filepath.Join(tdir, "lock"))
	os.Create(filepath.Join(tdir, "immortal.sock"))
	os.Create(filepath.Join(tdir, "f3"))
	files, _ := ioutil.ReadDir(tdir)
	expect(t, 3, len(files))
	err = ctl.PurgeServices(tdir)
	if err == nil {
		t.Error("Expecting and error")
	}
	files, _ = ioutil.ReadDir(tdir)
	expect(t, 3, len(files))
	err = ctl.PurgeServices(filepath.Join(tdir, "f3"))
	if err == nil {
		t.Error("Expecting and error")
	}
	os.Remove(filepath.Join(tdir, "f3"))
	err = ctl.PurgeServices(filepath.Join(tdir, "f3"))
	if err != nil {
		t.Error(err)
	}
	files, _ = ioutil.ReadDir(tdir)
	expect(t, 0, len(files))
	tdir = filepath.Join(dir, "test", "root")
	os.MkdirAll(tdir, 0700)
	os.Create(filepath.Join(dir, "test", "lock"))
	err = ctl.PurgeServices(filepath.Join(dir, "test", "immortal.sock"))
	if err == nil {
		t.Error("Expecting and error")
	}
	err = ctl.PurgeServices("/dev/null/non-existent")
	if err == nil {
		t.Error("Expecting and error")
	}
}

func TestRun(t *testing.T) {
	ctl := &Controller{}
	out, err := ctl.Run("echo -n immortal")
	if err != nil {
		t.Fatal(err)
	}
	expect(t, "immortal", string(out))
	_, err = ctl.Run("/dev/null/non-existent -n immortal")
	if err == nil {
		t.Fatal("Expecting an errro")
	}
}
