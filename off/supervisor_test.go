package immortal

import (
	"io/ioutil"
	"os"
	"testing"
)

func TestReadPidFileNonexistent(t *testing.T) {
	sup := new(Sup)
	i, e := sup.ReadPidFile("nonexistent")
	if i != 0 {
		t.Errorf("Expecting: 0 got: %v", i)
	}
	if e == nil {
		t.Errorf("Expecting: no such file or directory")
	}
}

func TestReadPidFileBadContent(t *testing.T) {
	sup := new(Sup)
	i, e := sup.ReadPidFile("funcs.go")
	if i != 0 {
		t.Errorf("Expecting: 0 got: %v", i)
	}
	if e == nil {
		t.Errorf("Expecting: no such file or directory")
	}
}

func TestReadPidFile(t *testing.T) {
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
	sup := new(Sup)
	i, e := sup.ReadPidFile(tmpfile.Name())
	if i != 1234 {
		t.Errorf("Expecting: 1234 got: %v", i)
	}
	if e != nil {
		t.Error(e)
	}
}
