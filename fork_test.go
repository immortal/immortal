package immortal

import (
	"os"
	"testing"
)

func TestFork(t *testing.T) {
	f := Fork{}
	pid, err := f.Fork()
	if err != nil {
		t.Error(err)
	}
	if pid == os.Getpid() {
		t.Error("Expecting different pid")
	}
}

func TestForkErr(t *testing.T) {
	oldArgs := os.Args
	defer func() { os.Args = oldArgs }()
	os.Args = []string{""}
	f := Fork{}
	_, err := f.Fork()
	if err == nil {
		t.Error("Expecting error: fork/exec : no such file or directory")
	}
}
