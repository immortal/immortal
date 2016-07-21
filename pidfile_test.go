package immortal

import (
	"io/ioutil"
	"os"
	"testing"
)

func TestReadPidfileNonexistent(t *testing.T) {
	D := &Daemon{
		run: Run{
			FollowPid: "nonexistent",
		},
	}
	i, e := D.readPidfile()
	if i != 0 {
		t.Errorf("Expecting: 0 got: %v", i)
	}
	if e == nil {
		t.Errorf("Expecting: no such file or directory")
	}
}

func TestReadPidfileBadContent(t *testing.T) {
	D := &Daemon{
		run: Run{
			FollowPid: "pidfile_test.go",
		},
	}
	i, e := D.readPidfile()
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
	D := &Daemon{
		run: Run{
			FollowPid: tmpfile.Name(),
		},
	}
	i, e := D.readPidfile()
	if i != 1234 {
		t.Errorf("Expecting: 1234 got: %v", i)
	}
	if e != nil {
		t.Error(e)
	}
}

func TestWritePidNonexistent(t *testing.T) {
	D := &Daemon{}
	err := D.writePid("/dev/null/nonexistent", 1234)
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
	D := &Daemon{
		run: Run{
			FollowPid: tmpfile.Name(),
		},
	}
	err = D.writePid(tmpfile.Name(), 1234)
	if err != nil {
		t.Error(err)
	}
	i, e := D.readPidfile()
	if i != 1234 {
		t.Errorf("Expecting: 1234 got: %v", i)
	}
	if e != nil {
		t.Error(e)
	}
}
