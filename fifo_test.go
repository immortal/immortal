package immortal

import (
	"io/ioutil"
	"os"
	"path/filepath"
	"testing"
)

func TestMakeFIFOError(t *testing.T) {
	err := MakeFifo("/dev/null/pipe")
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
	err = MakeFifo(fifo)
	if err != nil {
		t.Error(err)
	}
	f, err := OpenFifo(fifo)
	if err != nil {
		f.Close()
		t.Error(err)
	}
	os.Chmod(dir, 0000)
	err = MakeFifo(fifo)
	if err == nil {
		t.Error("Expecting error")
	}
	f, err = OpenFifo(fifo)
	if err == nil {
		t.Error("Expecting error")
	}
}
