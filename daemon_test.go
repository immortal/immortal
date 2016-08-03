package immortal

import (
	"io/ioutil"
	"os"
	"path/filepath"
	"strconv"
	"strings"
	"testing"
)

func TestDaemonNewCtrl(t *testing.T) {
	dir, err := ioutil.TempDir("", "TestDaemonNewCtrl")
	if err != nil {
		t.Error(err)
	}
	defer os.RemoveAll(dir)
	cfg := &Config{
		Cwd:  dir,
		ctrl: true,
	}
	d, err := New(cfg)
	if err != nil {
		t.Error(err)
	}
	f, err := os.Stat(filepath.Join(dir, "supervise/control"))
	if f.Mode()&os.ModeType != os.ModeNamedPipe {
		t.Error("Expecting os.ModeNamePipe")
	}
	f, err = os.Stat(filepath.Join(dir, "supervise/ok"))
	if f.Mode()&os.ModeType != os.ModeNamedPipe {
		t.Error("Expecting os.ModeNamePipe")
	}
	if _, err = os.Stat(filepath.Join(dir, "supervise/lock")); err != nil {
		t.Error(err)
	}
	expect(t, uint32(0), d.count)
	expect(t, uint32(0), d.count_defer)
	// test lock
	_, err = New(cfg)
	if err == nil {
		t.Error("Expecting error: resource temporarily unavailable")
	}
}

func TestDaemonNewCtrlErr(t *testing.T) {
	dir, err := ioutil.TempDir("", "TestDaemonNewCtrlErr")
	if err != nil {
		t.Error(err)
	}
	defer os.RemoveAll(dir)
	os.Chmod(dir, 0000)
	cfg := &Config{
		Cwd:  dir,
		ctrl: true,
	}
	_, err = New(cfg)
	if err == nil {
		t.Error("Expecting error")
	}
}

func TestDaemonNewCtrlCwd(t *testing.T) {
	dir, err := ioutil.TempDir("", "TestDaemonNewCtrlCwd")
	if err != nil {
		t.Error(err)
	}
	defer os.RemoveAll(dir)
	cwd, err := os.Getwd()
	if err != nil {
		t.Error(err)
	}
	defer os.Chdir(cwd)
	if err := os.Chdir(dir); err != nil {
		t.Error(err)
	}
	cfg := &Config{
		ctrl: true,
	}
	d, err := New(cfg)
	if err != nil {
		t.Error(err)
	}
	f, err := os.Stat(filepath.Join(dir, "supervise/control"))
	if f.Mode()&os.ModeType != os.ModeNamedPipe {
		t.Error("Expecting os.ModeNamePipe")
	}
	f, err = os.Stat(filepath.Join(dir, "supervise/ok"))
	if f.Mode()&os.ModeType != os.ModeNamedPipe {
		t.Error("Expecting os.ModeNamePipe")
	}
	if _, err = os.Stat(filepath.Join(dir, "supervise/lock")); err != nil {
		t.Error(err)
	}
	expect(t, uint32(0), d.count)
	expect(t, uint32(0), d.count_defer)
	// test lock
	_, err = New(cfg)
	if err == nil {
		t.Error("Expecting error: resource temporarily unavailable")
	}
}

func TestWritePid(t *testing.T) {
	cfg := &Config{}
	d, err := New(cfg)
	if err != nil {
		t.Error(err)
	}
	tmpfile, err := ioutil.TempFile("", "TestWritePid")
	if err != nil {
		t.Error(err)
	}
	defer os.Remove(tmpfile.Name()) // clean up
	err = d.WritePid(tmpfile.Name(), 1234)
	if err != nil {
		t.Error(err)
	}
	content, err := ioutil.ReadFile(tmpfile.Name())
	if err != nil {
		t.Error(err)
	}
	lines := strings.Split(string(content), "\n")
	pid, err := strconv.Atoi(lines[0])
	if err != nil {
		t.Error(err)
	}
	expect(t, pid, 1234)
}

func TestWritePidErr(t *testing.T) {
	cfg := &Config{}
	d, err := New(cfg)
	if err != nil {
		t.Error(err)
	}
	tmpfile, err := ioutil.TempFile("", "TestWritePid")
	if err != nil {
		t.Error(err)
	}
	defer os.Remove(tmpfile.Name()) // clean up
	os.Chmod(tmpfile.Name(), 0444)
	err = d.WritePid(tmpfile.Name(), 1234)
	if err == nil {
		t.Error("Expecting error: permission denied")
	}
}
