// +build freebsd netbsd openbsd dragonfly darwin

package immortal

import (
	"io/ioutil"
	"os"
	"path/filepath"
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

// TestWathFile create a dummy file, do changes on it and WathFile should return
// the same file
func TestWathFile(t *testing.T) {
	dir, err := ioutil.TempDir("", "TestWathFile")
	if err != nil {
		t.Error(err)
	}
	defer os.RemoveAll(dir)
	s, err := NewScanDir(dir)
	if err != nil {
		t.Fatal(err)
	}
	file := filepath.Join(dir, "run.yml")
	if err = ioutil.WriteFile(file, []byte(""), 0644); err != nil {
		t.Fatal(err)
	}
	go s.WatchFile(file)
	go func() {
		time.Sleep(time.Second)
		if err = ioutil.WriteFile(file, []byte("--"), 0644); err != nil {
			t.Fatal(err)
		}
	}()
	select {
	case watch := <-s.watch:
		expect(t, file, watch)
	case <-time.After(time.Second * 2):
		t.Fatal("time out waiting for file to change")
	}
}

type mockController struct{}

func (mc *mockController) GetStatus(socket string) (*Status, error)                  { return &Status{}, nil }
func (mc *mockController) SendSignal(socket, signal string) (*SignalResponse, error) { return nil, nil }
func (mc *mockController) FindServices(dir string) ([]*ServiceStatus, error)         { return nil, nil }
func (mc *mockController) PurgeServices(dir string) error                            { return nil }
func (mc *mockController) Run(command string) ([]byte, error) {
	// TODO
	return nil, nil
}

func TestScandir(t *testing.T) {
	dir, err := ioutil.TempDir("", "TestWathFile")
	if err != nil {
		t.Error(err)
	}
	defer os.RemoveAll(dir)
	s, err := NewScanDir(dir)
	if err != nil {
		t.Fatal(err)
	}
	dir, err = filepath.EvalSymlinks(dir)
	if err != nil {
		t.Fatal(err)
	}
	file := filepath.Join(dir, "run.yml")
	if err = ioutil.WriteFile(file, []byte(""), 0644); err != nil {
		t.Fatal(err)
	}
	go func() {
		time.Sleep(time.Second)
		if err = ioutil.WriteFile(file, []byte("--"), 0644); err != nil {
			t.Fatal(err)
		}
	}()
	ctl := &mockController{}
	s.Scandir(ctl)
	select {
	case watch := <-s.watch:
		expect(t, file, watch)
	case <-time.After(time.Second * 2):
		t.Fatal("time out waiting for file to change")
	}
	val, ok := s.services.Load("run")
	if !ok {
		t.Fatalf("Not expecting value for %s", "run.yml")
	}
	expect(t, val, "d41d8cd98f00b204e9800998ecf8427e")
}
