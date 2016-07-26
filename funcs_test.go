package immortal

import (
	"io/ioutil"
	"os"
	"path/filepath"
	"testing"
)

func TestLogo(t *testing.T) {
	logo := Logo()
	if logo != 11093 {
		t.Errorf("Expecting: 11093 got: %v", logo)
	}
}

func TestIconOk(t *testing.T) {
	i := Icon("1F621")
	if i != 128545 {
		t.Errorf("Expecting: 128545 got: %v", i)
	}
}

func TestIconErr(t *testing.T) {
	i := Icon(" ")
	if i != 0 {
		t.Errorf("Expecting: 0 got: %v", i)
	}
}

func TestReadPidfileNonexistent(t *testing.T) {
	i, e := ReadPidfile("nonexistent")
	if i != 0 {
		t.Errorf("Expecting: 0 got: %v", i)
	}
	if e == nil {
		t.Errorf("Expecting: no such file or directory")
	}
}

func TestReadPidfileBadContent(t *testing.T) {
	i, e := ReadPidfile("funcs.go")
	if i != 0 {
		t.Errorf("Expecting: 0 got: %v", i)
	}
	if e == nil {
		t.Errorf("Expecting: no such file or directory")
	}
}

func TestReadPidfile(t *testing.T) {
	content := []byte("1234")
	tmpfile, err := ioutil.TempFile("", "TestReadPidfile")
	if err != nil {
		t.Error(err)
	}

	defer os.Remove(tmpfile.Name()) // clean up

	if _, err := tmpfile.Write(content); err != nil {
		t.Error(err)
	}
	if err := tmpfile.Close(); err != nil {
		t.Error(err)
	}
	i, e := ReadPidfile(tmpfile.Name())
	if i != 1234 {
		t.Errorf("Expecting: 1234 got: %v", i)
	}
	if e != nil {
		t.Error(e)
	}
}

func TestWritePidNonexistent(t *testing.T) {
	err := WritePid("/dev/null/nonexistent", 1234)
	if err == nil {
		t.Error("Expecting an error")
	}
}

func TestWritePid(t *testing.T) {
	tmpfile, err := ioutil.TempFile("", "TestWritePid")
	if err != nil {
		t.Error(err)
	}
	defer os.Remove(tmpfile.Name()) // clean up
	err = WritePid(tmpfile.Name(), 1234)
	if err != nil {
		t.Error(err)
	}
	i, e := ReadPidfile(tmpfile.Name())
	if i != 1234 {
		t.Errorf("Expecting: 1234 got: %v", i)
	}
	if e != nil {
		t.Error(e)
	}
}

func TestLockNonexistent(t *testing.T) {
	err := Lock("/dev/null/nonexistent")
	if err == nil {
		t.Error("Expecting error")
	}
}

func TestLock(t *testing.T) {
	tmpfile, err := ioutil.TempFile("", "TestLock")
	if err != nil {
		t.Error(err)
	}
	defer os.Remove(tmpfile.Name()) // clean up
	err = Lock(tmpfile.Name())
	if err != nil {
		t.Error(err)
	}
	err = Lock(tmpfile.Name())
	if err == nil {
		t.Error("Expecting error")
	}
}

func TestMakeFIFOError(t *testing.T) {
	_, err := MakeFIFO("/dev/null/pipe")
	if err == nil {
		t.Error("Expecting error")
	}
}

func TestMakeFIFO(t *testing.T) {
	dir, err := ioutil.TempDir("", "TestMakeFIFO")
	if err != nil {
		t.Error(err)
	}
	defer os.RemoveAll(dir)
	fifo := filepath.Join(dir, "pipe")
	f, err := MakeFIFO(fifo)
	if err != nil {
		t.Error(err)
	}
	fi, err := f.Stat()
	if err != nil {
		f.Close()
		t.Error(err)
	}
	if fi.Mode()&os.ModeType != os.ModeNamedPipe {
		f.Close()
		t.Error("Expecting os.ModeNamePipe")
	}
	os.Chmod(fifo, 0000)
	f, err = MakeFIFO(fifo)
	if err == nil {
		t.Error("Expecting error")
	}
}

func TestGetEnv(t *testing.T) {
	dir, err := ioutil.TempDir("", "TestGetEnv")
	if err != nil {
		t.Error(err)
	}
	defer os.RemoveAll(dir)
	e1 := []byte("bar\naaa\nbbb")
	err = ioutil.WriteFile(filepath.Join(dir, "foo"), e1, 0644)
	if err != nil {
		t.Error(err)
	}
	e2 := []byte("PONG")
	err = ioutil.WriteFile(filepath.Join(dir, "PING"), e2, 0644)
	if err != nil {
		t.Error(err)
	}
	env, err := GetEnv(dir)
	if err != nil {
		t.Error(err)
	}
	var envTest = []struct {
		key      string
		expected string
	}{
		{"foo", "bar"},
		{"PING", "PONG"},
	}
	for _, tt := range envTest {
		if env[tt.key] != tt.expected {
			t.Errorf("For %s expected %s, actual %s", tt.key, tt.expected, env[tt.key])
		}
	}
}
